//! WFB auto-pair supervisor.
//!
//! This runs in the cloud relay (not in the radio services) on purpose: the
//! bind orchestrator stops + starts the wfb units to flip wfb-ng profiles, so a
//! supervisor hosted inside the service it is stopping would self-kill. The
//! cloud relay does not touch the radio, so it can drive the bind without dying.
//! Ports `src/ados/services/wfb/auto_pair.py`.
//!
//! The bind itself lives in the supervisor process; this forwards `start_bind`
//! over the supervisor control socket (`/run/ados/supervisor.sock`), matching
//! the bind-client seam: one newline-JSON request → one newline-JSON response.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// The supervisor control socket. Mirrors `SUPERVISOR_SOCK` on both sides.
pub const SUPERVISOR_SOCK: &str = "/run/ados/supervisor.sock";

/// Settle delay before the first bind attempt. Mirrors `START_DELAY_S`.
pub const START_DELAY: Duration = Duration::from_secs(15);

/// Backoff between attempts. Mirrors `RETRY_BACKOFF_S`.
pub const RETRY_BACKOFF: Duration = Duration::from_secs(60);

/// The error string the control socket returns when a bind already runs.
pub const E_BIND_IN_PROGRESS: &str = "E_BIND_IN_PROGRESS";

/// The outcome of a forwarded `start_bind`.
#[derive(Debug, PartialEq, Eq)]
pub enum BindOutcome {
    /// The bind completed; the session JSON is the orchestrator's result.
    Ok(serde_json::Value),
    /// Another bind path is already running (`E_BIND_IN_PROGRESS`); defer.
    Busy,
    /// The control socket returned an error string, or the request failed.
    Error(String),
}

/// Parse a `start_bind` reply line into an outcome. Mirrors the bind-client
/// `_parse_start_reply`: `{"ok":true,"session":{...}}` → Ok(session);
/// `{"ok":false,"error":"E_BIND_IN_PROGRESS"}` → Busy; any other error → Error.
pub fn parse_start_reply(line: &[u8]) -> BindOutcome {
    #[derive(Deserialize)]
    struct Reply {
        #[serde(default)]
        ok: bool,
        #[serde(default)]
        session: serde_json::Value,
        #[serde(default)]
        error: Option<String>,
    }
    let reply: Reply = match serde_json::from_slice(line) {
        Ok(r) => r,
        Err(e) => return BindOutcome::Error(format!("bad reply: {e}")),
    };
    if reply.ok {
        return BindOutcome::Ok(reply.session);
    }
    match reply.error.as_deref() {
        Some(E_BIND_IN_PROGRESS) => BindOutcome::Busy,
        Some(e) => BindOutcome::Error(e.to_string()),
        None => BindOutcome::Error("control socket returned not-ok with no error".to_string()),
    }
}

/// Forward a `start_bind` to the supervisor control socket and await the reply.
/// Sends one newline-JSON request, reads the newline-JSON response. The request
/// carries `source: "auto"` so the orchestrator logs the auto-pair origin.
/// Mirrors `forward_start_bind`.
pub async fn forward_start_bind(sock_path: &Path, role: &str) -> BindOutcome {
    let mut stream = match UnixStream::connect(sock_path).await {
        Ok(s) => s,
        Err(e) => return BindOutcome::Error(format!("connect failed: {e}")),
    };
    let req = serde_json::json!({
        "op": "start_bind",
        "role": role,
        "source": "auto",
    });
    let mut body = serde_json::to_vec(&req).unwrap_or_default();
    body.push(b'\n');
    if let Err(e) = stream.write_all(&body).await {
        return BindOutcome::Error(format!("write failed: {e}"));
    }
    if stream.flush().await.is_err() {
        return BindOutcome::Error("flush failed".to_string());
    }
    // Read until newline (the bind blocks for the whole rendezvous, so this can
    // be a long wait; the caller's cancel tears down the connection).
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.contains(&b'\n') {
                    break;
                }
            }
            Err(e) => return BindOutcome::Error(format!("read failed: {e}")),
        }
    }
    let line = match buf.iter().position(|&b| b == b'\n') {
        Some(i) => &buf[..i],
        None => &buf[..],
    };
    if line.is_empty() {
        return BindOutcome::Error("control socket closed before replying".to_string());
    }
    parse_start_reply(line)
}

/// Whether auto-pair should attempt a bind this tick: the role is bindable, the
/// config flag is armed, and the rig is not already paired. Mirrors the
/// per-iteration gate in `_run` (role check + `auto_pair_enabled` + not-paired).
pub fn should_attempt(role: &str, auto_pair_enabled: bool, already_paired: bool) -> bool {
    matches!(role, "drone" | "gs") && auto_pair_enabled && !already_paired
}

/// The default control-socket path.
pub fn default_sock_path() -> PathBuf {
    PathBuf::from(SUPERVISOR_SOCK)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ok_reply_yields_session() {
        let line = br#"{"ok":true,"session":{"id":"s1","state":"bound"}}"#;
        match parse_start_reply(line) {
            BindOutcome::Ok(s) => assert_eq!(s["id"], "s1"),
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn parse_busy_reply() {
        let line = br#"{"ok":false,"error":"E_BIND_IN_PROGRESS"}"#;
        assert_eq!(parse_start_reply(line), BindOutcome::Busy);
    }

    #[test]
    fn parse_error_reply() {
        let line = br#"{"ok":false,"error":"E_BAD_ROLE"}"#;
        assert_eq!(
            parse_start_reply(line),
            BindOutcome::Error("E_BAD_ROLE".to_string())
        );
    }

    #[test]
    fn parse_malformed_reply_is_error() {
        assert!(matches!(
            parse_start_reply(b"not json"),
            BindOutcome::Error(_)
        ));
    }

    #[test]
    fn should_attempt_gate() {
        assert!(should_attempt("drone", true, false));
        assert!(should_attempt("gs", true, false));
        // Disarmed.
        assert!(!should_attempt("drone", false, false));
        // Already paired.
        assert!(!should_attempt("drone", true, true));
        // Non-bindable role.
        assert!(!should_attempt("ground-station", true, false));
    }

    #[tokio::test]
    async fn forward_start_bind_round_trips_against_a_fake_socket() {
        // Stand up a fake control socket that replies with a busy line, and
        // confirm the client parses it.
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("supervisor.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            // Drain the request line.
            let mut buf = [0u8; 256];
            let _ = s.read(&mut buf).await;
            s.write_all(b"{\"ok\":false,\"error\":\"E_BIND_IN_PROGRESS\"}\n")
                .await
                .unwrap();
            s.flush().await.unwrap();
        });
        let outcome = forward_start_bind(&sock, "drone").await;
        assert_eq!(outcome, BindOutcome::Busy);
        server.abort();
    }
}
