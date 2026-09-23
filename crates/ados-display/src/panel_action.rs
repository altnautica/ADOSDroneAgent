//! Performs the agent writes the panel's controls ask for.
//!
//! A page resolves a tap on one of its controls to a
//! [`crate::pages::AgentRequest`] (method, route, JSON body). [`AgentWriter`]
//! sends it to the agent's local API with the same `X-ADOS-Key` the status polls
//! use and turns the answer into a one-line [`Outcome`] the panel shows, so a tap
//! is never a silent no-op: it either acknowledges or says why it failed.

use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use crate::pages::AgentRequest;
use crate::state_source::{load_api_key, DEFAULT_API_BASE, PAIRING_JSON_PATH};

/// Ceiling on one write. Longer than the 0.9 s status poll: a write may apply a
/// radio setting or restart a unit before it answers.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// What a write came to, for the panel's acknowledgement line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub ok: bool,
    pub message: String,
}

/// Sends panel writes to the agent's local API.
pub struct AgentWriter {
    base: String,
    api_key: Option<String>,
    agent: ureq::Agent,
}

impl AgentWriter {
    /// A writer against the local agent, keyed from `/etc/ados/pairing.json`.
    pub fn local() -> Self {
        Self::with_base(DEFAULT_API_BASE, Path::new(PAIRING_JSON_PATH))
    }

    /// A writer against an explicit base URL and pairing file (tests).
    pub fn with_base(base: impl Into<String>, pairing_json: &Path) -> Self {
        Self {
            base: base.into().trim_end_matches('/').to_string(),
            api_key: load_api_key(pairing_json),
            agent: ureq::AgentBuilder::new().timeout(WRITE_TIMEOUT).build(),
        }
    }

    /// Perform `req` and report the outcome. Blocking; the service runs it off
    /// the render loop.
    pub fn send(&self, req: &AgentRequest) -> Outcome {
        let url = format!("{}{}", self.base, req.path);
        let mut call = self.agent.request(req.method, &url);
        if let Some(key) = &self.api_key {
            call = call.set("X-ADOS-Key", key);
        }
        let result = match &req.body {
            Some(body) => call.send_json(body.clone()),
            None => call.call(),
        };
        match result {
            Ok(resp) => {
                let body = resp.into_json::<Value>().unwrap_or(Value::Null);
                match refusal(&body) {
                    Some(why) => failed(&req.label, &why),
                    None => Outcome {
                        ok: true,
                        message: format!("{}: done", req.label),
                    },
                }
            }
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_json::<Value>().unwrap_or(Value::Null);
                let why = error_text(&body).unwrap_or_else(|| format!("HTTP {code}"));
                failed(&req.label, &why)
            }
            Err(ureq::Error::Transport(_)) => failed(&req.label, "agent unreachable"),
        }
    }
}

fn failed(label: &str, why: &str) -> Outcome {
    Outcome {
        ok: false,
        message: format!("{label} failed: {why}"),
    }
}

/// A 2xx body that nevertheless reports the write did not take: `ok`,
/// `persisted` or `accepted` false. Returns the reason when there is one.
fn refusal(body: &Value) -> Option<String> {
    let refused = ["ok", "persisted", "accepted"]
        .iter()
        .any(|k| body.get(*k) == Some(&Value::Bool(false)));
    refused.then(|| error_text(body).unwrap_or_else(|| "refused".to_string()))
}

/// The human-readable reason in an agent error body, whichever of its shapes it
/// uses (`detail`, `detail.error.message`, `error`, `reason`, `persist_error`).
fn error_text(body: &Value) -> Option<String> {
    let s = |v: Option<&Value>| v.and_then(Value::as_str).map(str::to_string);
    s(body.pointer("/detail/error/message"))
        .or_else(|| s(body.pointer("/detail/error/code")))
        .or_else(|| s(body.get("detail")))
        .or_else(|| s(body.pointer("/error/message")))
        .or_else(|| s(body.get("error")))
        .or_else(|| s(body.get("reason")))
        .or_else(|| s(body.get("persist_error")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    /// Serve one request with `status` and `body`; return the raw request.
    fn serve_once(status: &str, body: &str) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (status, body) = (status.to_string(), body.to_string());
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut head = String::new();
            let mut len = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
                head.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            let mut req_body = vec![0u8; len];
            reader.read_exact(&mut req_body).unwrap();
            let reply = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(reply.as_bytes()).unwrap();
            head + &String::from_utf8(req_body).unwrap()
        });
        (base, handle)
    }

    fn writer(base: &str, dir: &Path) -> AgentWriter {
        let pairing = dir.join("pairing.json");
        std::fs::write(&pairing, r#"{"api_key":"k-123"}"#).unwrap();
        AgentWriter::with_base(base, &pairing)
    }

    #[test]
    fn a_write_carries_the_key_method_route_and_body() {
        let dir = tempfile::tempdir().unwrap();
        let (base, server) = serve_once("200 OK", r#"{"requested_dbm":9,"persisted":true}"#);
        let out = writer(&base, dir.path()).send(&AgentRequest {
            method: "PUT",
            path: "/api/wfb/tx-power",
            body: Some(json!({"tx_power_dbm": 9})),
            label: "TX power 9 dBm".to_string(),
        });
        let raw = server.join().unwrap();
        assert!(
            raw.starts_with("PUT /api/wfb/tx-power HTTP/1.1\r\n"),
            "{raw}"
        );
        assert!(
            raw.to_ascii_lowercase().contains("x-ados-key: k-123"),
            "{raw}"
        );
        assert!(raw.ends_with(r#"{"tx_power_dbm":9}"#), "{raw}");
        assert_eq!(
            out,
            Outcome {
                ok: true,
                message: "TX power 9 dBm: done".to_string()
            }
        );
    }

    /// A 200 that says the write did not take is a failure on the panel, and an
    /// error status shows the agent's reason.
    #[test]
    fn refusals_and_errors_are_reported_as_failures() {
        let dir = tempfile::tempdir().unwrap();
        let (base, server) = serve_once("200 OK", r#"{"ok":false,"error":"systemctl missing"}"#);
        let out = writer(&base, dir.path()).send(&AgentRequest {
            method: "POST",
            path: "/api/v1/system/restart-supervisor",
            body: None,
            label: "Restart agent".to_string(),
        });
        server.join().unwrap();
        assert_eq!(out.message, "Restart agent failed: systemctl missing");
        assert!(!out.ok);

        let (base, server) = serve_once(
            "409 Conflict",
            r#"{"detail":{"error":{"code":"E_RECORDING_NOT_ACTIVE"}}}"#,
        );
        let out = writer(&base, dir.path()).send(&AgentRequest {
            method: "POST",
            path: "/api/v1/ground-station/recording/stop",
            body: None,
            label: "Stop recording".to_string(),
        });
        server.join().unwrap();
        assert_eq!(out.message, "Stop recording failed: E_RECORDING_NOT_ACTIVE");

        let out = AgentWriter::with_base("http://127.0.0.1:1", Path::new("/nonexistent")).send(
            &AgentRequest {
                method: "DELETE",
                path: "/api/v1/ground-station/wfb/pair",
                body: None,
                label: "Unpair".to_string(),
            },
        );
        assert_eq!(out.message, "Unpair failed: agent unreachable");
    }
}
