//! The ground-station data-plane command-socket client.
//!
//! The mesh-write and WFB-pair routes have no in-process pair/role manager to
//! call from the front; the data-plane service (`ados-groundlink`) owns the role
//! transition, the gateway preference, and the WFB rx-key install/unpair, and
//! exposes them on a Unix command socket. This module is the small client both
//! route modules forward through: one newline-terminated JSON request in, one
//! newline-terminated JSON reply out, then the server closes — the same framing
//! the radio + Wi-Fi command-socket clients use.
//!
//! The reply is returned raw (with its transport `ok` flag intact); each route
//! maps `ok:true`/`ok:false` to its own response shape. An unreachable socket / a
//! read error / a closed-before-reply / an unparseable reply all yield `None` so
//! the caller can take its no-link posture (a 503 — the front cannot drive the
//! systemd/`batctl` work itself).

use std::path::PathBuf;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A command reply is a few hundred bytes; bound the read to guard a runaway.
const MAX_REPLY_BYTES: usize = 64 * 1024;

/// The data-plane command socket (`/run/ados/groundlink-cmd.sock`), honouring
/// `ADOS_RUN_DIR` (the same override the sibling sockets + sidecars resolve
/// under). Mirrors the `GROUNDLINK_CMD_SOCK` constant the service binds.
fn groundlink_cmd_sock() -> PathBuf {
    PathBuf::from(std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string()))
        .join("groundlink-cmd.sock")
}

/// Send one newline-terminated JSON request to the data-plane command socket and
/// read one newline-terminated JSON reply. Returns the parsed reply (its `ok` flag
/// intact), or `None` on an unreachable socket / a write or read error / a closed
/// connection before a reply / an unparseable or non-object reply, so the caller
/// can branch to its no-link posture. The read is bounded so a runaway reply
/// cannot exhaust memory.
pub async fn groundlink_cmd_roundtrip(request: &Value) -> Option<Value> {
    let mut stream = tokio::net::UnixStream::connect(groundlink_cmd_sock())
        .await
        .ok()?;
    let mut line = serde_json::to_vec(request).ok()?;
    line.push(b'\n');
    if stream.write_all(&line).await.is_err() || stream.flush().await.is_err() {
        return None;
    }

    let mut raw = Vec::new();
    let mut buf = [0u8; 8 * 1024];
    loop {
        let n = stream.read(&mut buf).await.ok()?;
        if n == 0 {
            break; // EOF: the server replies once then closes.
        }
        if raw.len() + n > MAX_REPLY_BYTES {
            return None;
        }
        raw.extend_from_slice(&buf[..n]);
        // The reply is one newline-terminated line; stop at the first newline.
        if raw.contains(&b'\n') {
            break;
        }
    }
    if raw.is_empty() {
        // The socket closed before replying.
        return None;
    }
    let text = String::from_utf8(raw).ok()?;
    let first = text.lines().next()?;
    let parsed: Value = serde_json::from_str(first).ok()?;
    parsed.is_object().then_some(parsed)
}
