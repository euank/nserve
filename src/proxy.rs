use std::{convert::Infallible, time::Instant};

use bytes::Bytes;
use futures_util::TryStreamExt;
use http_body_util::{combinators::UnsyncBoxBody, BodyExt, Full};
use hyper::{
    body::Incoming,
    header::{CACHE_CONTROL, CONNECTION, CONTENT_TYPE, RETRY_AFTER, UPGRADE},
    service::service_fn,
    Method, Request, Response, StatusCode, Uri,
};
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::{TokioExecutor, TokioIo},
};
use ngrok::prelude::ConnInfo;
use tokio::{io::copy_bidirectional, net::TcpStream, sync::watch};

use crate::{
    ingress::Ingress,
    state::{AppState, RequestRecord},
};

type ProxyBody = UnsyncBoxBody<Bytes, hyper::Error>;

pub async fn run(
    ingress: Ingress,
    upstream: watch::Receiver<Option<u16>>,
    state: AppState,
) -> anyhow::Result<()> {
    match ingress {
        Ingress::Http(tunnel) => http(tunnel, upstream, state).await,
        Ingress::Tcp(tunnel) => tcp(tunnel, upstream, state).await,
    }
}

async fn http(
    mut tunnel: ngrok::tunnel::HttpTunnel,
    upstream: watch::Receiver<Option<u16>>,
    state: AppState,
) -> anyhow::Result<()> {
    let client: Client<HttpConnector, Incoming> =
        Client::builder(TokioExecutor::new()).build_http();
    while let Some(conn) = tunnel.try_next().await? {
        let client = client.clone();
        let state = state.clone();
        let upstream = upstream.clone();
        tokio::spawn(async move {
            let service = service_fn(move |request| {
                proxy_request(request, client.clone(), upstream.clone(), state.clone())
            });
            if let Err(error) = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(conn), service)
                .with_upgrades()
                .await
            {
                eprintln!("[nserve] HTTP connection failed: {error}");
            }
        });
    }
    state.set_session("closed");
    Ok(())
}

async fn proxy_request(
    mut request: Request<Incoming>,
    client: Client<HttpConnector, Incoming>,
    upstream: watch::Receiver<Option<u16>>,
    state: AppState,
) -> Result<Response<ProxyBody>, Infallible> {
    let started = Instant::now();
    let method = request.method().clone();
    let original_uri = request.uri().to_string();

    let Some(port) = *upstream.borrow() else {
        let status = StatusCode::SERVICE_UNAVAILABLE;
        let response = waiting_response(method == Method::HEAD);
        record_request(&state, method, original_uri, status, started);
        return Ok(response);
    };

    let path = request
        .uri()
        .path_and_query()
        .map_or("/", |value| value.as_str());
    let target: Uri = match format!("http://localhost:{port}{path}").parse() {
        Ok(uri) => uri,
        Err(error) => {
            let status = StatusCode::BAD_REQUEST;
            let response = error_response(status, format!("invalid upstream URI: {error}"));
            record_request(&state, method, original_uri, status, started);
            return Ok(response);
        }
    };
    *request.uri_mut() = target;

    let wants_upgrade = request.headers().contains_key(UPGRADE)
        || request
            .headers()
            .get(CONNECTION)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.to_ascii_lowercase().contains("upgrade"));
    let downstream_upgrade = wants_upgrade.then(|| hyper::upgrade::on(&mut request));

    let (response, status) = match client.request(request).await {
        Ok(mut response) => {
            let status = response.status();
            if status == StatusCode::SWITCHING_PROTOCOLS {
                if let Some(downstream) = downstream_upgrade {
                    let upstream = hyper::upgrade::on(&mut response);
                    tokio::spawn(async move {
                        if let (Ok(downstream), Ok(upstream)) = (downstream.await, upstream.await) {
                            let mut downstream = TokioIo::new(downstream);
                            let mut upstream = TokioIo::new(upstream);
                            let _ = copy_bidirectional(&mut downstream, &mut upstream).await;
                        }
                    });
                }
            }
            (response.map(|body| body.boxed_unsync()), status)
        }
        Err(_) => {
            let status = StatusCode::SERVICE_UNAVAILABLE;
            (waiting_response(method == Method::HEAD), status)
        }
    };

    record_request(&state, method, original_uri, status, started);
    Ok(response)
}

fn record_request(
    state: &AppState,
    method: Method,
    uri: String,
    status: StatusCode,
    started: Instant,
) {
    state.record(RequestRecord {
        at: chrono::Local::now(),
        method: method.to_string(),
        uri,
        status: status.as_u16(),
        elapsed: started.elapsed(),
    });
}

fn waiting_response(head_only: bool) -> Response<ProxyBody> {
    const PAGE: &str = r#"<!doctype html>
<html lang="en">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Starting local server…</title>
<style>
  :root { color-scheme: light dark; font-family: system-ui, sans-serif }
  body { min-height: 100vh; margin: 0; display: grid; place-items: center }
  main { text-align: center }
  .spinner { width: 2.5rem; height: 2.5rem; margin: 0 auto 1.25rem; border: .3rem solid color-mix(in srgb, CanvasText 20%, transparent); border-top-color: CanvasText; border-radius: 50%; animation: spin .8s linear infinite }
  @keyframes spin { to { transform: rotate(360deg) } }
</style>
<main><div class="spinner" aria-hidden="true"></div><h1>Starting the local server…</h1><p>This page will retry automatically.</p></main>
<script>setTimeout(() => location.reload(), 1000)</script>
</html>"#;

    let contents = if head_only { "" } else { PAGE };
    let body = Full::new(Bytes::from(contents))
        .map_err(|never| match never {})
        .boxed_unsync();
    Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .header(CONTENT_TYPE, "text/html; charset=utf-8")
        .header(CACHE_CONTROL, "no-store")
        .header(RETRY_AFTER, "1")
        .header("refresh", "1")
        .body(body)
        .expect("valid waiting response")
}

fn error_response(status: StatusCode, message: String) -> Response<ProxyBody> {
    let body = Full::new(Bytes::from(message))
        .map_err(|never| match never {})
        .boxed_unsync();
    Response::builder()
        .status(status)
        .body(body)
        .expect("valid error response")
}

async fn tcp(
    mut tunnel: ngrok::tunnel::TcpTunnel,
    upstream: watch::Receiver<Option<u16>>,
    state: AppState,
) -> anyhow::Result<()> {
    while let Some(mut conn) = tunnel.try_next().await? {
        let remote = conn.remote_addr();
        let mut upstream_state = upstream.clone();
        tokio::spawn(async move {
            let port = loop {
                if let Some(port) = *upstream_state.borrow() {
                    break port;
                }
                if upstream_state.changed().await.is_err() {
                    return;
                }
            };
            match TcpStream::connect(("localhost", port)).await {
                Ok(mut upstream) => {
                    let _ = copy_bidirectional(&mut conn, &mut upstream).await;
                }
                Err(error) => {
                    eprintln!("[nserve] TCP upstream failed for {remote}: {error}");
                }
            }
        });
    }
    state.set_session("closed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waiting_page_is_retryable_and_not_cached() {
        let response = waiting_response(false);
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[RETRY_AFTER], "1");
        assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
        assert_eq!(response.headers()["refresh"], "1");
    }
}
