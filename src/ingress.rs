use anyhow::{bail, Context, Result};
use ngrok::{
    config::{Scheme, TunnelBuilder},
    prelude::EndpointInfo,
    tunnel::{HttpTunnel, TcpTunnel},
};
use serde_json::json;
use url::Url;

use crate::{auth, cli::Cli};

pub enum Ingress {
    Http(HttpTunnel),
    Tcp(TcpTunnel),
}

impl Ingress {
    pub fn url(&self) -> &str {
        match self {
            Self::Http(tunnel) => tunnel.url(),
            Self::Tcp(tunnel) => tunnel.url(),
        }
    }
}

pub async fn connect(cli: &Cli) -> Result<Ingress> {
    let mut session_builder = ngrok::Session::builder();
    if let Some(token) = auth::authtoken()? {
        session_builder.authtoken(token);
    } else {
        session_builder.authtoken_from_env();
    }
    let session = session_builder
        .connect()
        .await
        .context(
            "failed to connect an embedded ngrok session (set NGROK_AUTHTOKEN or configure ~/.config/ngrok/ngrok.yml)",
        )?;

    if cli.tcp {
        let mut builder = session.tcp_endpoint();
        builder.forwards_to("nserve child process (waiting for listener)");
        if let Some(requested) = &cli.url {
            builder.remote_addr(parse_tcp_address(requested)?);
        }
        Ok(Ingress::Tcp(
            builder
                .listen()
                .await
                .context("failed to create TCP endpoint")?,
        ))
    } else {
        let mut builder = session.http_endpoint();
        builder.forwards_to("nserve child process (waiting for listener)");
        if let Some(requested) = &cli.url {
            let (domain, scheme) = parse_http_url(requested)?;
            builder.domain(domain).scheme(scheme);
        }
        if let Some(pattern) = &cli.google_oauth {
            builder.traffic_policy(google_oauth_policy(pattern)?);
        }
        Ok(Ingress::Http(
            builder
                .listen()
                .await
                .context("failed to create HTTP endpoint")?,
        ))
    }
}

fn parse_http_url(value: &str) -> Result<(String, Scheme)> {
    let normalized = if value.contains("://") {
        value.to_owned()
    } else {
        format!("https://{value}")
    };
    let parsed = Url::parse(&normalized).context("--url must be an HTTP(S) URL or domain")?;
    let scheme = match parsed.scheme() {
        "http" => Scheme::HTTP,
        "https" => Scheme::HTTPS,
        other => bail!("HTTP mode does not support a {other} --url"),
    };
    if parsed.path() != "/" || parsed.query().is_some() || parsed.fragment().is_some() {
        bail!("--url may not contain a path, query, or fragment");
    }
    let domain = parsed
        .host_str()
        .context("--url is missing a domain")?
        .to_owned();
    Ok((domain, scheme))
}

fn parse_tcp_address(value: &str) -> Result<String> {
    let address = value.strip_prefix("tcp://").unwrap_or(value);
    if address.is_empty() || !address.contains(':') {
        bail!("TCP --url must look like tcp://host:port");
    }
    Ok(address.to_owned())
}

fn google_oauth_policy(pattern: &str) -> Result<String> {
    if pattern.is_empty() {
        bail!("--google-oauth requires a non-empty email regex");
    }
    let quoted_pattern = serde_json::to_string(pattern)?;
    Ok(serde_json::to_string(&json!({
        "on_http_request": [
            {
                "actions": [{
                    "type": "oauth",
                    "config": { "provider": "google" }
                }]
            },
            {
                "expressions": [format!(
                    "!actions.ngrok.oauth.identity.email.matches({quoted_pattern})"
                )],
                "actions": [{ "type": "deny" }]
            }
        ]
    }))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_requested_http_url() {
        let (domain, scheme) = parse_http_url("http://demo.ngrok.app").unwrap();
        assert_eq!(domain, "demo.ngrok.app");
        assert!(matches!(scheme, Scheme::HTTP));
    }

    #[test]
    fn policy_escapes_user_regex() {
        let policy = google_oauth_policy(r#"^a\"b@example\.com$"#).unwrap();
        let value: serde_json::Value = serde_json::from_str(&policy).unwrap();
        assert_eq!(value["on_http_request"][0]["actions"][0]["type"], "oauth");
        assert!(value["on_http_request"][1]["expressions"][0]
            .as_str()
            .unwrap()
            .contains(r#"a\\\"b"#));
    }
}
