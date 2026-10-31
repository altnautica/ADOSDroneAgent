//! Primary-gamepad resolution through the input daemon.

use super::*;

/// Resolve a gamepad device id (`usb:<vid>:<pid>:<eventN>`) to its evdev node
/// path. The trailing segment IS the `/dev/input` basename.
pub fn device_path_from_id(device_id: &str) -> Option<String> {
    let basename = device_id.rsplit(':').next()?;
    if !basename.starts_with("event") || basename.len() <= 5 {
        return None;
    }
    if !basename[5..].bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(format!("/dev/input/{basename}"))
}

/// The `ados-input` daemon's command socket under the resolved run dir.
pub fn hid_cmd_sock_path() -> PathBuf {
    PathBuf::from(crate::paths::run_path("hid-cmd.sock"))
}

/// Ask the input daemon which gamepad is primary: one newline-JSON
/// `get_primary` round-trip. `None` when the socket is unreachable, the reply
/// is unparseable/`ok:false`, or no primary is bound.
pub async fn query_primary(socket: &Path) -> Option<String> {
    pub(super) const MAX_REPLY_BYTES: usize = 64 * 1024;

    let request = json!({"op": "get_primary"});
    let mut stream = tokio::net::UnixStream::connect(socket).await.ok()?;
    let line = format!("{}\n", serde_json::to_string(&request).ok()?);
    stream.write_all(line.as_bytes()).await.ok()?;
    stream.flush().await.ok()?;

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
        if raw.contains(&b'\n') {
            break;
        }
    }
    let text = String::from_utf8(raw).ok()?;
    let reply: Value = serde_json::from_str(text.lines().next()?).ok()?;
    if reply.get("ok") != Some(&Value::Bool(true)) {
        return None;
    }
    reply
        .get("primary_id")
        .and_then(Value::as_str)
        .map(str::to_string)
}
