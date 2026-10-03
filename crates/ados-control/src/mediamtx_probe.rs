//! Loopback reads of the local mediamtx: a one-shot HTTP/1.1 `GET` and the
//! readiness verdict for the primary `main` path.
//!
//! The primary stream is always published at the fixed `main` path, and that is
//! the path `/whep` and `/hls/main/index.m3u8` address. Readiness is therefore
//! judged on `main` alone: a secondary leg (`eo_wide`, `ir`, a plugin camera)
//! being ready says nothing about whether the advertised URL plays. A path is
//! ready only when mediamtx reports `ready: true` AND a non-null `source` (a
//! publisher is attached); an absent `main` is not ready.

use ados_video::mediamtx::DEFAULT_API_PORT;
use serde_json::{Map, Value};

/// A minimal HTTP/1.1 `GET` over a local TCP endpoint, returning the status code
/// and the decoded body. mediamtx speaks HTTP on loopback TCP ports, not a Unix
/// socket. `Connection: close` reads the body to EOF; a chunked body is
/// de-chunked. Bounded in size and in time (2 s) so a runaway or stalled peer
/// cannot hold the handler.
pub(crate) async fn http_get_local(url: &str) -> std::io::Result<(u16, Vec<u8>)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::time::{timeout, Duration};

    const MAX_READ_BYTES: usize = 4 * 1024 * 1024;
    const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

    // Parse `http://host:port/path` into the connect target + the request path.
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| std::io::Error::other("non-http url"))?;
    let (authority, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };
    let path = if path.is_empty() { "/" } else { path };

    let fut = async {
        let mut stream = tokio::net::TcpStream::connect(authority).await?;
        let head = format!("GET {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n");
        stream.write_all(head.as_bytes()).await?;
        stream.flush().await?;

        let mut raw = Vec::new();
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = stream.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            if raw.len() + n > MAX_READ_BYTES {
                return Err(std::io::Error::other("probe response too large"));
            }
            raw.extend_from_slice(&buf[..n]);
        }
        crate::ipc::logd_client::parse_http_response(&raw)
    };

    match timeout(PROBE_TIMEOUT, fut).await {
        Ok(res) => res,
        Err(_) => Err(std::io::Error::other("probe timed out")),
    }
}

/// The management API's `/v3/paths/list` body, or `None` when mediamtx is
/// unreachable, answers non-200 (the ground-station mediamtx puts credentials on
/// this API), or returns a body that is not JSON.
pub(crate) async fn read_paths_list() -> Option<Value> {
    let url = format!("http://127.0.0.1:{DEFAULT_API_PORT}/v3/paths/list");
    let (status, body) = http_get_local(&url).await.ok()?;
    if status != 200 {
        return None;
    }
    serde_json::from_slice(&body).ok()
}

/// The `main` path object out of a `/v3/paths/list` body, looked up by name.
/// There is deliberately no fallback to another path: `None` when `main` is not
/// listed, whatever else is.
pub(crate) fn main_path(list: &Value) -> Option<&Map<String, Value>> {
    list.get("items")?
        .as_array()?
        .iter()
        .filter_map(Value::as_object)
        .find(|p| p.get("name").and_then(Value::as_str) == Some("main"))
}

/// True when a mediamtx path object reports a live publisher: `ready: true` and
/// a non-null `source`.
pub(crate) fn path_ready(path: &Map<String, Value>) -> bool {
    path.get("ready").and_then(Value::as_bool).unwrap_or(false)
        && path.get("source").is_some_and(|s| !s.is_null())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_ready_secondary_path_does_not_stand_in_for_an_absent_main() {
        let list = json!({"items": [
            {"name": "eo_wide", "ready": true, "source": {"type": "rpiCameraSource"}},
        ]});
        assert!(main_path(&list).is_none());
    }

    #[test]
    fn main_is_found_by_name_wherever_it_is_listed() {
        let list = json!({"items": [
            {"name": "ir", "ready": true, "source": {"type": "rtspSource"}},
            {"name": "main", "ready": false, "source": null},
        ]});
        let main = main_path(&list).expect("main is listed");
        assert_eq!(main.get("name"), Some(&json!("main")));
        assert!(!path_ready(main));
    }

    #[test]
    fn ready_requires_both_the_flag_and_a_publisher() {
        let ready = |v: Value| path_ready(v.as_object().unwrap());
        assert!(ready(
            json!({"ready": true, "source": {"type": "webRTCSession"}})
        ));
        // mediamtx can report ready on a path whose publisher just left.
        assert!(!ready(json!({"ready": true, "source": null})));
        assert!(!ready(json!({"ready": true})));
        assert!(!ready(
            json!({"ready": false, "source": {"type": "rtspSession"}})
        ));
    }
}
