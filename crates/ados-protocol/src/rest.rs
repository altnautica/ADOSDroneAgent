//! REST client for the agent HTTP API (Contract D, port 8080).
//!
//! The FastAPI REST layer stays Python; this is the client side used by the
//! Rust TUI (and any other Rust REST consumer). Auth is the `X-ADOS-Key`
//! header; the pairing routes are reachable while unpaired. The client is
//! blocking (`ureq`, HTTP-only) because its callers are simple pollers off the
//! flight-critical path.
//!
//! Responses are returned as `serde_json::Value` for now. A typed client
//! generated from the FastAPI `app.openapi()` schema is the eventual path; this
//! hand-written client covers the endpoints the TUI consumes today.

use std::time::Duration;

use serde_json::Value;
use thiserror::Error;

/// Default base URL: the agent's local HTTP API. Matches `cli/main.py`'s
/// `API_BASE`.
pub const DEFAULT_BASE: &str = "http://localhost:8080";

/// Default per-request timeout, matching the Python CLI's httpx client.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Debug, Error)]
pub enum RestError {
    #[error("http {0}")]
    Status(u16),
    #[error("transport error: {0}")]
    Transport(String),
    #[error("response was not valid json: {0}")]
    Json(String),
}

/// A blocking REST client for the agent HTTP API.
pub struct RestClient {
    base: String,
    api_key: Option<String>,
    agent: ureq::Agent,
}

impl RestClient {
    /// Build a client for the given base URL (no trailing slash).
    pub fn new(base: impl Into<String>) -> Self {
        let agent = ureq::AgentBuilder::new().timeout(DEFAULT_TIMEOUT).build();
        Self {
            base: base.into().trim_end_matches('/').to_string(),
            api_key: None,
            agent,
        }
    }

    /// Client for the default local agent (`http://localhost:8080`).
    pub fn local() -> Self {
        Self::new(DEFAULT_BASE)
    }

    /// Attach the pairing key sent as `X-ADOS-Key` on every request.
    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    /// GET `path` (e.g. `/api/status`) and parse the JSON body.
    pub fn get_json(&self, path: &str) -> Result<Value, RestError> {
        let url = format!("{}{}", self.base, path);
        let mut req = self.agent.get(&url);
        if let Some(ref key) = self.api_key {
            req = req.set("X-ADOS-Key", key);
        }
        match req.call() {
            Ok(resp) => resp
                .into_json::<Value>()
                .map_err(|e| RestError::Json(e.to_string())),
            Err(ureq::Error::Status(code, _resp)) => Err(RestError::Status(code)),
            Err(ureq::Error::Transport(t)) => Err(RestError::Transport(t.to_string())),
        }
    }

    /// GET `/api/status` — the agent status snapshot the dashboard renders.
    pub fn status(&self) -> Result<Value, RestError> {
        self.get_json("/api/status")
    }

    /// GET `/api/v1/setup/status` — the setup wizard state (read-only).
    pub fn setup_status(&self) -> Result<Value, RestError> {
        self.get_json("/api/v1/setup/status")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// Spawn a one-shot HTTP server that returns `body` with `status`, and
    /// return the base URL pointing at it.
    fn one_shot_server(status_line: &'static str, body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf); // consume the request line + headers
                let response = format!(
                    "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        format!("http://{addr}")
    }

    #[test]
    fn get_json_parses_a_200_response() {
        let base = one_shot_server("200 OK", r#"{"armed":false,"mode":"STABILIZE"}"#);
        let client = RestClient::new(base);
        let v = client.status().unwrap();
        assert_eq!(v["mode"], "STABILIZE");
        assert_eq!(v["armed"], false);
    }

    #[test]
    fn non_2xx_maps_to_status_error() {
        let base = one_shot_server("401 Unauthorized", r#"{"detail":"missing key"}"#);
        let client = RestClient::new(base).with_api_key("wrong");
        match client.status() {
            Err(RestError::Status(401)) => {}
            other => panic!("expected 401 status error, got {other:?}"),
        }
    }

    #[test]
    fn transport_error_when_nothing_listens() {
        // Port 1 is privileged and nothing listens; connect fails fast.
        let client = RestClient::new("http://127.0.0.1:1");
        assert!(matches!(client.status(), Err(RestError::Transport(_))));
    }
}
