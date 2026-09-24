//! One-shot exchanges with the sibling daemons' command sockets.
//!
//! Every command socket the front forwards a write to (radio, net, groundlink,
//! video, PIC, CRSF, input, relay tunnel, pairing) speaks the same framing: one
//! newline-terminated JSON request, one newline-terminated JSON reply, then the
//! server closes. This is the one implementation of that exchange, and it is
//! bounded end to end: a daemon that accepts the connection and then stalls
//! costs the caller its deadline, not a handler, a task and a file descriptor
//! held for ever.
//!
//! The deadline is the caller's, because the daemons' own work differs by two
//! orders of magnitude: a PIC claim answers in milliseconds, a Wi-Fi join waits
//! on NetworkManager for up to 30 s before it replies.

use std::path::Path;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// The deadline for a command whose daemon answers from memory (a status read,
/// an arbiter decision, a primary-device pick).
pub const QUICK: Duration = Duration::from_secs(5);

/// A reply is a few hundred bytes; the cap only guards a runaway peer.
const MAX_REPLY_BYTES: usize = 1024 * 1024;

/// Why a command-socket exchange produced no reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmdFailure {
    /// Nothing is listening: the socket is absent or refuses the connection.
    Unreachable,
    /// The daemon accepted but did not answer within the deadline.
    Timeout,
    /// The exchange broke, or the reply was empty, oversized, or not UTF-8.
    BadReply,
}

/// Send `request` as one line and return the first line of the reply, all
/// within `bound`.
pub async fn roundtrip_line(
    socket: &Path,
    request: &Value,
    bound: Duration,
) -> Result<String, CmdFailure> {
    let mut line = serde_json::to_vec(request).map_err(|_| CmdFailure::BadReply)?;
    line.push(b'\n');
    let exchange = async {
        let mut stream = UnixStream::connect(socket)
            .await
            .map_err(|_| CmdFailure::Unreachable)?;
        stream
            .write_all(&line)
            .await
            .map_err(|_| CmdFailure::BadReply)?;
        stream.flush().await.map_err(|_| CmdFailure::BadReply)?;

        let mut raw = Vec::new();
        let mut buf = [0u8; 8 * 1024];
        loop {
            let n = stream
                .read(&mut buf)
                .await
                .map_err(|_| CmdFailure::BadReply)?;
            if n == 0 {
                break; // EOF: the server replies once then closes.
            }
            if raw.len() + n > MAX_REPLY_BYTES {
                return Err(CmdFailure::BadReply);
            }
            raw.extend_from_slice(&buf[..n]);
            if raw.contains(&b'\n') {
                break;
            }
        }
        let text = String::from_utf8(raw).map_err(|_| CmdFailure::BadReply)?;
        match text.lines().next() {
            Some(first) if !first.trim().is_empty() => Ok(first.to_string()),
            _ => Err(CmdFailure::BadReply),
        }
    };
    tokio::time::timeout(bound, exchange)
        .await
        .unwrap_or(Err(CmdFailure::Timeout))
}

/// [`roundtrip_line`] with the reply decoded as JSON.
pub async fn roundtrip(
    socket: &Path,
    request: &Value,
    bound: Duration,
) -> Result<Value, CmdFailure> {
    let line = roundtrip_line(socket, request, bound).await?;
    serde_json::from_str(&line).map_err(|_| CmdFailure::BadReply)
}

/// [`roundtrip`], keeping only a JSON-object reply.
pub async fn roundtrip_object(
    socket: &Path,
    request: &Value,
    bound: Duration,
) -> Result<serde_json::Map<String, Value>, CmdFailure> {
    match roundtrip(socket, request, bound).await? {
        Value::Object(map) => Ok(map),
        _ => Err(CmdFailure::BadReply),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::net::UnixListener;

    #[tokio::test]
    async fn a_reply_line_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cmd.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (mut conn, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 256];
            let n = conn.read(&mut buf).await.unwrap();
            assert_eq!(&buf[..n], b"{\"op\":\"status\"}\n");
            conn.write_all(b"{\"ok\":true}\n").await.unwrap();
        });
        let reply = roundtrip(&path, &json!({"op": "status"}), QUICK)
            .await
            .unwrap();
        assert_eq!(reply, json!({"ok": true}));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn an_absent_socket_is_unreachable() {
        let dir = tempfile::tempdir().unwrap();
        let err = roundtrip(&dir.path().join("absent.sock"), &json!({}), QUICK)
            .await
            .unwrap_err();
        assert_eq!(err, CmdFailure::Unreachable);
    }

    #[tokio::test(start_paused = true)]
    async fn a_daemon_that_accepts_and_stalls_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cmd.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (conn, _) = listener.accept().await.unwrap();
            tokio::time::sleep(QUICK * 10).await;
            drop(conn);
        });
        let err = roundtrip(&path, &json!({"op": "status"}), QUICK)
            .await
            .unwrap_err();
        assert_eq!(err, CmdFailure::Timeout);
        server.abort();
    }

    #[tokio::test]
    async fn a_close_without_a_reply_is_a_bad_reply() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cmd.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (conn, _) = listener.accept().await.unwrap();
            drop(conn);
        });
        let err = roundtrip(&path, &json!({}), QUICK).await.unwrap_err();
        assert_eq!(err, CmdFailure::BadReply);
        server.await.unwrap();
    }
}
