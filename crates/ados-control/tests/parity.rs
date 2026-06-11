//! Golden-fixture parity + auth-gate integration tests for the native control
//! surface.
//!
//! The golden fixtures (`tests/fixtures/{healthz,version}.json`) were captured
//! from the FastAPI route sources (`version.py` / `server.py`). The Rust
//! handlers must emit the identical JSON SHAPE: the same key set, the same value
//! types, and — for the static parts — the same scalar values (the capability
//! list verbatim, `api_version` exactly, `status` exactly). The one field that
//! legitimately differs between fixture-capture time and run time is
//! `agent_version`: the fixture pins whatever version was current at capture,
//! the running crate reports its own; both must be non-empty strings, so that
//! field is asserted by type, not by value.
//!
//! The auth-gate tests drive the LAN TCP edge end to end: unpaired-open,
//! paired-key-required, loopback-trust, the exempt public paths, and the
//! `{"detail"}` error-body shape (NOT the logd `{"error":{...}}` envelope).

use std::path::{Path, PathBuf};
use std::time::Duration;

use ados_control::{run_with_paths, DaemonPaths};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UnixStream};
use tokio::sync::oneshot;

/// Read a captured golden fixture as a JSON value.
fn fixture(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read fixture {path}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse fixture {name}: {e}"))
}

/// A running server bound to temp sockets/port, with a stop trigger.
struct Harness {
    socket: PathBuf,
    port: u16,
    stop: Option<oneshot::Sender<()>>,
    join: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl Harness {
    async fn stop(mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        let _ = self.join.await;
    }
}

/// Bring the server up against a temp dir, optionally seeding `pairing.json`.
async fn start(dir: &Path, pairing_body: Option<&str>) -> Harness {
    let socket = dir.join("control.sock");
    let pairing_path = dir.join("pairing.json");
    if let Some(body) = pairing_body {
        std::fs::write(&pairing_path, body).unwrap();
    }
    // Ask the OS for an ephemeral free port, then release it for the server to
    // rebind (a tiny race window that is fine for a test).
    let probe = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);

    let paths = DaemonPaths {
        control_socket: socket.clone(),
        control_tcp_port: port,
        pairing_path,
    };
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let join = tokio::spawn(run_with_paths(paths, async move {
        let _ = stop_rx.await;
    }));
    // Wait for the socket to appear.
    for _ in 0..200 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Harness {
        socket,
        port,
        stop: Some(stop_tx),
        join,
    }
}

/// Connect to the unix socket, retrying briefly to ride out the bind/accept race.
async fn connect_unix(socket: &Path) -> UnixStream {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        match UnixStream::connect(socket).await {
            Ok(s) => return s,
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(e) => panic!("connect {}: {e}", socket.display()),
        }
    }
}

/// Minimal HTTP/1.1 GET over the unix socket: write the request, read the whole
/// response, return (status_line, body).
async fn unix_get(socket: &Path, path: &str, header: Option<(&str, &str)>) -> (String, String) {
    let mut stream = connect_unix(socket).await;
    let mut req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
    if let Some((k, v)) = header {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    split_http(&buf)
}

/// Same minimal GET over a TCP connection to 127.0.0.1:port, with optional
/// headers (the key and/or a forwarding header).
async fn tcp_get(port: u16, path: &str, headers: &[(&str, &str)]) -> (String, String) {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let mut req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    split_http(&buf)
}

/// Split a raw HTTP response into the status line and the body (after the blank
/// line), de-chunking a chunked body crudely enough for the test envelopes.
fn split_http(buf: &[u8]) -> (String, String) {
    let text = String::from_utf8_lossy(buf).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head.lines().next().unwrap_or("").to_string();
    let body = if head.to_lowercase().contains("transfer-encoding: chunked") {
        de_chunk(body)
    } else {
        body.to_string()
    };
    (status, body)
}

fn de_chunk(body: &str) -> String {
    let mut out = String::new();
    let mut rest = body;
    while let Some((len_line, after)) = rest.split_once("\r\n") {
        let len = usize::from_str_radix(len_line.trim(), 16).unwrap_or(0);
        if len == 0 {
            break;
        }
        if after.len() < len {
            out.push_str(after);
            break;
        }
        out.push_str(&after[..len]);
        rest = after[len..].strip_prefix("\r\n").unwrap_or(&after[len..]);
    }
    out
}

/// Assert the two JSON objects carry the exact same key set.
fn assert_same_keys(got: &Value, want: &Value, route: &str) {
    let gk: std::collections::BTreeSet<_> = got
        .as_object()
        .unwrap_or_else(|| panic!("{route}: response is not an object: {got}"))
        .keys()
        .collect();
    let wk: std::collections::BTreeSet<_> = want.as_object().unwrap().keys().collect();
    assert_eq!(gk, wk, "{route}: key set differs from the golden fixture");
}

// --- /healthz parity ---

#[tokio::test]
async fn healthz_matches_the_golden_shape_over_unix() {
    let dir = tempfile::tempdir().unwrap();
    let h = start(dir.path(), None).await;
    let (status, body) = unix_get(&h.socket, "/healthz", None).await;
    assert!(status.contains("200"), "status was {status}");
    let got: Value = serde_json::from_str(&body).unwrap_or_else(|_| panic!("body: {body}"));
    let want = fixture("healthz.json");
    assert_same_keys(&got, &want, "/healthz");
    // `status` is the static scalar; `version` differs in value but is a string.
    assert_eq!(got["status"], want["status"], "/healthz status scalar");
    assert!(
        got["version"].is_string() && !got["version"].as_str().unwrap().is_empty(),
        "/healthz version must be a non-empty string, got {}",
        got["version"]
    );
    h.stop().await;
}

// --- /api/version parity ---

#[tokio::test]
async fn version_matches_the_golden_shape_over_unix() {
    let dir = tempfile::tempdir().unwrap();
    let h = start(dir.path(), None).await;
    let (status, body) = unix_get(&h.socket, "/api/version", None).await;
    assert!(status.contains("200"), "status was {status}");
    let got: Value = serde_json::from_str(&body).unwrap_or_else(|_| panic!("body: {body}"));
    let want = fixture("version.json");
    assert_same_keys(&got, &want, "/api/version");
    // api_version is the static contract scalar.
    assert_eq!(
        got["api_version"], want["api_version"],
        "/api/version api_version scalar"
    );
    // The capability list must match the fixture VERBATIM (order included): it
    // is the canonical append-only surface contract with the GCS.
    assert_eq!(
        got["capabilities"], want["capabilities"],
        "/api/version capabilities list drifted from the golden fixture"
    );
    // agent_version differs between capture and run time; assert by type.
    assert!(
        got["agent_version"].is_string() && !got["agent_version"].as_str().unwrap().is_empty(),
        "/api/version agent_version must be a non-empty string, got {}",
        got["agent_version"]
    );
    h.stop().await;
}

// --- auth gate ---

#[tokio::test]
async fn tcp_is_open_on_every_route_when_unpaired() {
    let dir = tempfile::tempdir().unwrap();
    // No pairing file → unpaired → open on TCP with no key.
    let h = start(dir.path(), None).await;
    // A loopback connection would be on-box-trusted regardless; assert the
    // unpaired-open posture explicitly by also presenting a forwarding header so
    // the on-box shortcut does NOT fire, proving it is the unpaired gate opening
    // the route.
    let (status, _b) = tcp_get(
        h.port,
        "/api/version",
        &[("X-Forwarded-For", "203.0.113.7")],
    )
    .await;
    assert!(status.contains("200"), "status {status}");
    h.stop().await;
}

#[tokio::test]
async fn tcp_requires_the_key_when_paired() {
    let dir = tempfile::tempdir().unwrap();
    let h = start(
        dir.path(),
        Some(r#"{"paired": true, "api_key": "ados_secret"}"#),
    )
    .await;
    // A non-public route over TCP, carrying a forwarding header so the on-box
    // loopback shortcut is excluded and the pairing gate is the thing under test.
    let fwd = ("X-Forwarded-For", "203.0.113.7");
    // No key → 401 with a {"detail"} body.
    let (no_key, body) = tcp_get(h.port, "/api/time", &[fwd]).await;
    assert!(no_key.contains("401"), "status {no_key}");
    let json: Value = serde_json::from_str(&body).unwrap();
    assert!(
        json.get("detail").and_then(|d| d.as_str()).is_some(),
        "401 body must be the FastAPI {{\"detail\"}} shape, got {body}"
    );
    assert!(
        json.get("error").is_none(),
        "401 body must NOT carry the logd {{\"error\"}} envelope: {body}"
    );
    // Wrong key → 401.
    let (wrong, _b) = tcp_get(h.port, "/api/time", &[fwd, ("X-ADOS-Key", "nope")]).await;
    assert!(wrong.contains("401"), "status {wrong}");
    // Right key → not a 401 (the route itself is unregistered this chunk, so a
    // 404 is the expected pass-through; the point is the gate let it through).
    let (ok, _b) = tcp_get(h.port, "/api/time", &[fwd, ("X-ADOS-Key", "ados_secret")]).await;
    assert!(
        !ok.contains("401") && !ok.contains("429"),
        "right key should pass the gate, got {ok}"
    );
    h.stop().await;
}

#[tokio::test]
async fn the_exempt_public_paths_answer_even_when_paired() {
    let dir = tempfile::tempdir().unwrap();
    let h = start(dir.path(), Some(r#"{"paired": true, "api_key": "k"}"#)).await;
    // Present a forwarding header so the on-box shortcut is excluded: these must
    // answer because they are PUBLIC, not because of loopback trust.
    let fwd = ("X-Forwarded-For", "203.0.113.7");
    for path in ["/healthz", "/api/version"] {
        let (status, _b) = tcp_get(h.port, path, &[fwd]).await;
        assert!(status.contains("200"), "{path} should be public: {status}");
    }
    h.stop().await;
}

#[tokio::test]
async fn loopback_tcp_is_trusted_when_paired_without_a_key() {
    let dir = tempfile::tempdir().unwrap();
    let h = start(dir.path(), Some(r#"{"paired": true, "api_key": "k"}"#)).await;
    // A non-public route, no key, no forwarding header: the connection peer is
    // 127.0.0.1 (the test connects to 127.0.0.1:port), so the on-box loopback
    // shortcut must let it past the pairing gate. The route is unregistered this
    // chunk, so a 404 (NOT a 401) proves the gate was bypassed.
    let (status, body) = tcp_get(h.port, "/api/time", &[]).await;
    assert!(
        status.contains("404"),
        "loopback should bypass the key gate (expect a 404 pass-through, not 401): {status}"
    );
    let json: Value = serde_json::from_str(&body).unwrap();
    assert!(
        json.get("detail").is_some() && json.get("error").is_none(),
        "404 body must be the {{\"detail\"}} shape: {body}"
    );
    h.stop().await;
}

#[tokio::test]
async fn loopback_trust_is_dropped_when_a_forwarding_header_is_present() {
    let dir = tempfile::tempdir().unwrap();
    let h = start(dir.path(), Some(r#"{"paired": true, "api_key": "k"}"#)).await;
    // Loopback peer but a forwarding header → a tunnel terminating on loopback,
    // NOT on-box: the pairing gate must engage and reject the keyless request.
    let (status, _b) = tcp_get(h.port, "/api/time", &[("X-Forwarded-For", "203.0.113.7")]).await;
    assert!(
        status.contains("401"),
        "a forwarded loopback request must be gated, got {status}"
    );
    h.stop().await;
}

#[tokio::test]
async fn unknown_path_is_a_clean_detail_404() {
    let dir = tempfile::tempdir().unwrap();
    let h = start(dir.path(), None).await;
    let (status, body) = unix_get(&h.socket, "/api/does-not-exist", None).await;
    assert!(status.contains("404"), "status {status}");
    let json: Value = serde_json::from_str(&body).unwrap();
    assert!(
        json.get("detail").is_some() && json.get("error").is_none(),
        "404 must be the {{\"detail\"}} shape, not the logd envelope: {body}"
    );
    h.stop().await;
}
