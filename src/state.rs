use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use chrono::{DateTime, Local};

const REQUEST_LIMIT: usize = 100;

#[derive(Clone, Debug)]
pub struct RequestRecord {
    pub at: DateTime<Local>,
    pub method: String,
    pub uri: String,
    pub status: u16,
    pub elapsed: Duration,
}

#[derive(Debug)]
struct Inner {
    url: String,
    local_port: Option<u16>,
    session: String,
    requests: VecDeque<RequestRecord>,
}

#[derive(Clone, Debug)]
pub struct AppState(Arc<Mutex<Inner>>);

impl AppState {
    pub fn new(url: String) -> Self {
        Self(Arc::new(Mutex::new(Inner {
            url,
            local_port: None,
            session: "connected".into(),
            requests: VecDeque::new(),
        })))
    }

    pub fn record(&self, record: RequestRecord) {
        let mut inner = self.0.lock().expect("status mutex poisoned");
        if inner.requests.len() == REQUEST_LIMIT {
            inner.requests.pop_front();
        }
        inner.requests.push_back(record);
    }

    pub fn set_session(&self, session: impl Into<String>) {
        self.0.lock().expect("status mutex poisoned").session = session.into();
    }

    pub fn set_local_port(&self, port: u16) {
        self.0.lock().expect("status mutex poisoned").local_port = Some(port);
    }

    pub fn render(&self) -> String {
        let inner = self.0.lock().expect("status mutex poisoned");
        let local = inner.local_port.map_or_else(
            || "waiting for child listener".to_owned(),
            |port| format!("http://localhost:{port}"),
        );
        let mut output = format!(
            "\r\n[nserve] status\r\n  URL: {}\r\n  Local: {}\r\n  Session: {}\r\n  Recent requests:\r\n",
            inner.url, local, inner.session
        );
        if inner.requests.is_empty() {
            output.push_str("    (none)\r\n");
        } else {
            for request in inner.requests.iter().rev() {
                output.push_str(&format!(
                    "    {} {:<7} {:<3} {:>6}ms {}\r\n",
                    request.at.format("%H:%M:%S"),
                    request.method,
                    request.status,
                    request.elapsed.as_millis(),
                    request.uri
                ));
            }
        }
        output
    }
}
