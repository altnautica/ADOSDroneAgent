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
use ados_protocol::state::{encode_v1, encode_v2};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UnixListener, UnixStream};
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
    start_with_state(dir, pairing_body, dir.join("state.sock")).await
}

/// Bring the server up with an explicit state-socket path so a test can point it
/// at a live mock-IPC server (or at an absent path for the degraded shape). The
/// MAVLink command socket points at an absent path (no command test here).
async fn start_with_state(
    dir: &Path,
    pairing_body: Option<&str>,
    state_socket: PathBuf,
) -> Harness {
    start_full(
        dir,
        pairing_body,
        state_socket,
        dir.join("absent-mavlink.sock"),
    )
    .await
}

/// Bring the server up with explicit state + MAVLink socket paths so a command
/// test can point the MAVLink socket at a live mock server (or an absent path for
/// the no-socket 503 case).
async fn start_full(
    dir: &Path,
    pairing_body: Option<&str>,
    state_socket: PathBuf,
    mavlink_socket: PathBuf,
) -> Harness {
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
        pairing_path: pairing_path.clone(),
        dashboard_pin_path: dir.join("dashboard-pin.json"),
        mcp_token_path: dir.join("mcp-token.json"),
        state_socket,
        mavlink_socket,
        // Point the pairing-route reads at the same temp dir so a test can seed a
        // config / wfb key / bind-state sidecar; absent files degrade to the
        // documented defaults (board "unknown", empty device id, null bind_state).
        config_path: dir.join("config.yaml"),
        wfb_key_dir: dir.join("wfb"),
        bind_state_path: dir.join("bind-state.json"),
        // The status route's health source + board sidecar, in the same temp dir.
        // Absent by default → health degrades to the zero default, board to `{}`;
        // a test seeds a mock query server / a board.json to exercise the present
        // paths.
        logd_query_socket: dir.join("logd-query.sock"),
        board_path: dir.join("board.json"),
        // The pairing-info profile resolver's sentinels, in the same temp dir.
        // Absent by default → the resolver falls back to the config's explicit
        // profile; a test seeds `mesh-role` to exercise a ground-station role.
        profile_conf_path: dir.join("profile.conf"),
        mesh_role_path: dir.join("mesh-role"),
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
    // shortcut must let it past the pairing gate.
    //
    // Use a genuinely-unregistered path so a 404 (NOT a 401) cleanly proves the
    // gate was bypassed rather than the route answering. (The path is non-public
    // and non-existent, so the only way to a 404 is past the pairing gate.)
    let (status, body) = tcp_get(h.port, "/api/unregistered-probe", &[]).await;
    assert!(
        status.contains("404"),
        "loopback should bypass the key gate (expect a 404 pass-through, not 401): {status}"
    );
    let json: Value = serde_json::from_str(&body).unwrap();
    assert!(
        json.get("detail").is_some() && json.get("error").is_none(),
        "404 body must be the {{\"detail\"}} shape: {body}"
    );
    // And the now-live, non-exempt /api/time answers 200 over the same trusted
    // loopback path without a key, confirming the gate bypass reaches a real route.
    let (live, _b) = tcp_get(h.port, "/api/time", &[]).await;
    assert!(
        live.contains("200"),
        "loopback trust should reach the live /api/time without a key: {live}"
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

// --- mock state-socket IPC server ---

/// A discriminator for the value type at a JSON leaf, used to compare a response
/// against a golden fixture by shape (key set + value types) rather than by
/// scalar value (the live status/telemetry/time values change per run).
fn json_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Assert that two JSON values have the same shape: identical key sets at every
/// object level and the same value kind at every leaf. Numbers are not split into
/// int/float since serde_json unifies them and the wire does too.
fn assert_same_shape(got: &Value, want: &Value, path: &str) {
    assert_eq!(
        json_kind(got),
        json_kind(want),
        "{path}: kind differs (got {}, want {})",
        json_kind(got),
        json_kind(want)
    );
    match (got, want) {
        (Value::Object(g), Value::Object(w)) => {
            let gk: std::collections::BTreeSet<_> = g.keys().collect();
            let wk: std::collections::BTreeSet<_> = w.keys().collect();
            assert_eq!(gk, wk, "{path}: object key set differs");
            for (k, wv) in w {
                assert_same_shape(&g[k], wv, &format!("{path}.{k}"));
            }
        }
        (Value::Array(g), Value::Array(w)) => {
            // Compare element kinds against the fixture's first element (the
            // arrays here are homogeneous: rc.channels ints, cell_voltages).
            if let Some(w0) = w.first() {
                for (i, gv) in g.iter().enumerate() {
                    assert_same_shape(gv, w0, &format!("{path}[{i}]"));
                }
            }
        }
        _ => {}
    }
}

/// A mock state-socket server that accepts one connection, pushes a single canned
/// snapshot frame in the requested wire format, then idles (holding the
/// connection open) until the test drops it. Mirrors the real producer's
/// push-on-connect behaviour.
struct MockStateServer {
    path: PathBuf,
    stop: Option<oneshot::Sender<()>>,
    join: tokio::task::JoinHandle<()>,
}

/// The wire format the mock pushes its canned snapshot in.
#[derive(Clone, Copy)]
enum Wire {
    V1Json,
    V2Msgpack,
}

impl MockStateServer {
    /// Bind a state socket in `dir` and start serving the canned `snapshot` in
    /// the chosen wire format. The first accepted client gets the frame on
    /// connect.
    async fn start(dir: &Path, snapshot: Value, wire: Wire) -> Self {
        let path = dir.join("mock-state.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let frame = match wire {
            Wire::V1Json => encode_v1(&snapshot).unwrap(),
            Wire::V2Msgpack => encode_v2(&snapshot).unwrap(),
        };
        let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
        let join = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut stop_rx => return,
                    accepted = listener.accept() => {
                        if let Ok((mut conn, _addr)) = accepted {
                            let _ = conn.write_all(&frame).await;
                            let _ = conn.flush().await;
                            // Hold the connection open so the client keeps the
                            // snapshot; a fresh accept loop serves a reconnect.
                            tokio::spawn(async move {
                                let mut sink = [0u8; 64];
                                loop {
                                    match conn.read(&mut sink).await {
                                        Ok(0) | Err(_) => return,
                                        Ok(_) => {}
                                    }
                                }
                            });
                        }
                    }
                }
            }
        });
        Self {
            path,
            stop: Some(stop_tx),
            join,
        }
    }

    async fn stop(mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        let _ = self.join.await;
    }
}

/// The canned snapshot the mock pushes: the vehicle-state keys (the `to_wire`
/// shape) plus the four runtime-only extras the producer merges on top.
fn canned_snapshot() -> Value {
    fixture("state_snapshot.json")
}

/// Poll a route until it returns a snapshot-sourced body (the FC triple reflects
/// the canned snapshot), riding out the brief window before the state client has
/// connected and decoded the first frame.
async fn poll_status_until_connected(socket: &Path) -> Value {
    for _ in 0..100 {
        let (status, body) = unix_get(socket, "/api/status", None).await;
        if status.contains("200") {
            if let Ok(v) = serde_json::from_str::<Value>(&body) {
                if v.get("fc_connected") == Some(&Value::Bool(true)) {
                    return v;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("status never reflected the canned snapshot");
}

// --- /api/time parity ---

#[tokio::test]
async fn time_matches_the_golden_shape_over_unix() {
    let dir = tempfile::tempdir().unwrap();
    let h = start(dir.path(), None).await;
    let (status, body) = unix_get(&h.socket, "/api/time", None).await;
    assert!(status.contains("200"), "status was {status}");
    let got: Value = serde_json::from_str(&body).unwrap_or_else(|_| panic!("body: {body}"));
    let want = fixture("time.json");
    assert_same_keys(&got, &want, "/api/time");
    // The clock values are live (they differ per call); assert by type. time_ns
    // and monotonic_ns are numbers, ntp_synced is a bool.
    assert!(got["time_ns"].is_number(), "time_ns must be a number");
    assert!(
        got["monotonic_ns"].is_number(),
        "monotonic_ns must be a number"
    );
    assert!(got["ntp_synced"].is_boolean(), "ntp_synced must be a bool");
    h.stop().await;
}

// --- /api/status parity (degraded: no state socket) ---

#[tokio::test]
async fn status_matches_the_golden_shape_with_no_state_socket() {
    let dir = tempfile::tempdir().unwrap();
    // Point the state socket at an absent path: the snapshot stays empty and the
    // route degrades to the disconnected shape (never a 500).
    let h = start_with_state(dir.path(), None, dir.path().join("absent-state.sock")).await;
    let (status, body) = unix_get(&h.socket, "/api/status", None).await;
    assert!(status.contains("200"), "status must be 200, was {status}");
    let got: Value = serde_json::from_str(&body).unwrap_or_else(|_| panic!("body: {body}"));
    let want = fixture("status.json");
    // The optional cameraState / cameraUsbRecovery keys are only-if-fresh; the
    // golden fixture has neither (no sidecars on the capture host), so the key
    // sets match exactly here.
    assert_same_keys(&got, &want, "/api/status");
    // Static + degraded value assertions.
    assert!(got["version"].is_string() && !got["version"].as_str().unwrap().is_empty());
    assert!(got["uptime_seconds"].is_number(), "uptime is a number");
    assert!(got["board"].is_object(), "board is an object");
    assert!(got["health"].is_object(), "health is an object");
    assert_eq!(
        got["fc_connected"],
        Value::Bool(false),
        "no snapshot → disconnected"
    );
    assert_eq!(got["fc_port"], Value::String(String::new()), "default port");
    assert_eq!(got["fc_baud"], serde_json::json!(0), "default baud");
    // The dependency map is a {name: bool} object with the five video binaries.
    let deps = got["dependencies"]
        .as_object()
        .expect("dependencies object");
    for name in [
        "mediamtx",
        "ffmpeg",
        "rpicam-vid",
        "v4l2-ctl",
        "gst-launch-1.0",
    ] {
        assert!(
            deps.get(name).map(Value::is_boolean).unwrap_or(false),
            "dependency {name} must be a bool"
        );
    }
    h.stop().await;
}

// --- /api/status board + health sourcing (mock store + seeded sidecar) ---

/// A mock logd query server: answers every `GET /v1/query?kind=hw` with a canned
/// hardware-snapshot envelope so the status route's health block can be asserted.
/// Accepts repeatedly (the client opens a fresh `Connection: close` per request).
struct MockLogdServer {
    stop: Option<oneshot::Sender<()>>,
    join: tokio::task::JoinHandle<()>,
}

impl MockLogdServer {
    async fn start(dir: &Path, hw_body: String) -> Self {
        let path = dir.join("logd-query.sock");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            hw_body.len(),
            hw_body
        );
        let join = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut stop_rx => break,
                    accepted = listener.accept() => {
                        if let Ok((mut conn, _addr)) = accepted {
                            let r = resp.clone();
                            tokio::spawn(async move {
                                let mut buf = [0u8; 1024];
                                let _ = conn.read(&mut buf).await;
                                let _ = conn.write_all(r.as_bytes()).await;
                                let _ = conn.flush().await;
                            });
                        }
                    }
                }
            }
        });
        for _ in 0..100 {
            if path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Self {
            stop: Some(stop_tx),
            join,
        }
    }

    async fn stop(mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        let _ = self.join.await;
    }
}

#[tokio::test]
async fn status_sources_health_and_board_from_the_store_and_sidecar() {
    let dir = tempfile::tempdir().unwrap();
    // Seed the board sidecar the status route reads the full HAL dict from.
    std::fs::write(
        dir.path().join("board.json"),
        r#"{"name":"rpi4b","soc":"BCM2711","arch":"aarch64","ram_mb":4096}"#,
    )
    .unwrap();
    // A mock store returning one hardware snapshot with the health-spine signals.
    let hw = serde_json::json!({
        "data": [{
            "id": 1, "ts_us": 1000, "signals": {
                "cpu.util.all": 23.4,
                "mem.total_bytes": 4_000_000_000.0_f64,
                "mem.avail_bytes": 1_000_000_000.0_f64,
                "disk.fs_total_bytes": 100_000_000_000.0_f64,
                "disk.fs_used_bytes": 40_000_000_000.0_f64,
                "thermal.primary_c": 51.2
            }
        }]
    })
    .to_string();
    let logd = MockLogdServer::start(dir.path(), hw).await;

    let h = start_with_state(dir.path(), None, dir.path().join("absent-state.sock")).await;
    let (status, body) = unix_get(&h.socket, "/api/status", None).await;
    assert!(status.contains("200"), "{status}");
    let got: Value = serde_json::from_str(&body).unwrap_or_else(|_| panic!("body: {body}"));

    // Board is the full sidecar dict, not the degraded empty object.
    assert_eq!(got["board"]["name"], serde_json::json!("rpi4b"));
    assert_eq!(got["board"]["soc"], serde_json::json!("BCM2711"));
    assert_eq!(got["board"]["ram_mb"], serde_json::json!(4096));

    // Health derives from the store signals: cpu passthrough, used/total ratios.
    let health = &got["health"];
    assert_eq!(health["cpu_percent"], serde_json::json!(23.4));
    assert_eq!(health["memory_percent"], serde_json::json!(75.0)); // 3e9/4e9
    assert_eq!(health["disk_percent"], serde_json::json!(40.0)); // 40e9/100e9
    assert_eq!(health["temperature"], serde_json::json!(51.2));
    assert!(
        health["timestamp"]
            .as_str()
            .map(|t| t.ends_with("+00:00") && t.contains('T'))
            .unwrap_or(false),
        "health timestamp must be an ISO-8601 UTC string"
    );

    h.stop().await;
    logd.stop().await;
}

// --- /api/status parity (live mock state socket, v1 JSON) ---

#[tokio::test]
async fn status_reads_the_fc_triple_from_a_v1_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let mock = MockStateServer::start(dir.path(), canned_snapshot(), Wire::V1Json).await;
    let h = start_with_state(dir.path(), None, mock.path.clone()).await;

    let got = poll_status_until_connected(&h.socket).await;
    // The FC triple + uptime now come from the snapshot extras.
    assert_eq!(got["fc_connected"], Value::Bool(true));
    assert_eq!(got["fc_port"], serde_json::json!("/dev/ttyACM0"));
    assert_eq!(got["fc_baud"], serde_json::json!(115200));
    assert_eq!(got["uptime_seconds"], serde_json::json!(99.0));
    // The full key set still matches the golden fixture.
    assert_same_keys(&got, &fixture("status.json"), "/api/status (snapshot)");

    h.stop().await;
    mock.stop().await;
}

// --- /api/telemetry parity (live mock, v1 + v2) ---

#[tokio::test]
async fn telemetry_projects_a_v1_snapshot_minus_the_four_extras() {
    telemetry_projection_for_wire(Wire::V1Json).await;
}

#[tokio::test]
async fn telemetry_projects_a_v2_msgpack_snapshot_minus_the_four_extras() {
    telemetry_projection_for_wire(Wire::V2Msgpack).await;
}

/// Shared body for the v1/v2 telemetry projection tests: bring up a mock pushing
/// the canned snapshot in the given wire format, poll telemetry until it reflects
/// the snapshot, and assert it equals the projected golden fixture (the four
/// extras stripped) by shape, with the four extras provably absent.
async fn telemetry_projection_for_wire(wire: Wire) {
    let dir = tempfile::tempdir().unwrap();
    let mock = MockStateServer::start(dir.path(), canned_snapshot(), wire).await;
    let h = start_with_state(dir.path(), None, mock.path.clone()).await;

    // Poll until the snapshot has propagated (telemetry is non-empty).
    let mut got = Value::Null;
    for _ in 0..100 {
        let (status, body) = unix_get(&h.socket, "/api/telemetry", None).await;
        if status.contains("200") {
            if let Ok(v) = serde_json::from_str::<Value>(&body) {
                if v.as_object().map(|m| !m.is_empty()).unwrap_or(false) {
                    got = v;
                    break;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        got.is_object() && !got.as_object().unwrap().is_empty(),
        "telemetry never reflected the snapshot"
    );

    let want = fixture("telemetry_from_snapshot.json");
    assert_same_keys(&got, &want, "/api/telemetry");
    assert_same_shape(&got, &want, "/api/telemetry");
    // The four runtime-only extras are provably absent from telemetry.
    let obj = got.as_object().unwrap();
    for k in ["fc_connected", "fc_port", "fc_baud", "service_uptime"] {
        assert!(!obj.contains_key(k), "{k} must be stripped from telemetry");
    }
    // The vehicle values survive verbatim.
    assert_eq!(got["mode"], serde_json::json!("GUIDED"));
    assert_eq!(got["armed"], Value::Bool(true));
    assert_eq!(got["battery"]["voltage"], serde_json::json!(16.4));

    h.stop().await;
    mock.stop().await;
}

// --- /api/telemetry parity (degraded: no state socket → {}) ---

#[tokio::test]
async fn telemetry_is_an_empty_object_with_no_state_socket() {
    let dir = tempfile::tempdir().unwrap();
    let h = start_with_state(dir.path(), None, dir.path().join("absent-state.sock")).await;
    let (status, body) = unix_get(&h.socket, "/api/telemetry", None).await;
    assert!(status.contains("200"), "status must be 200, was {status}");
    let got: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(got, serde_json::json!({}), "no snapshot → empty telemetry");
    h.stop().await;
}

// --- /api/time auth gate (NOT exempt) ---

#[tokio::test]
async fn time_is_gated_when_paired_but_answers_with_a_key() {
    let dir = tempfile::tempdir().unwrap();
    let h = start(dir.path(), Some(r#"{"paired": true, "api_key": "k"}"#)).await;
    let fwd = ("X-Forwarded-For", "203.0.113.7");
    // No key → 401 (proving /api/time is NOT in the exempt set).
    let (no_key, _b) = tcp_get(h.port, "/api/time", &[fwd]).await;
    assert!(no_key.contains("401"), "/api/time must be gated: {no_key}");
    // Right key → 200 with the time shape.
    let (ok, body) = tcp_get(h.port, "/api/time", &[fwd, ("X-ADOS-Key", "k")]).await;
    assert!(ok.contains("200"), "keyed /api/time should answer: {ok}");
    let got: Value = serde_json::from_str(&body).unwrap();
    assert!(got["time_ns"].is_number() && got["ntp_synced"].is_boolean());
    h.stop().await;
}

// --- pairing parity (R1: the highest-risk surface) ---

/// Minimal HTTP/1.1 POST over the unix socket with a JSON body: write the
/// request, read the whole response, return (status_line, body).
async fn unix_post(
    socket: &Path,
    path: &str,
    header: Option<(&str, &str)>,
    json_body: &str,
) -> (String, String) {
    let mut stream = connect_unix(socket).await;
    let mut req = format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        json_body.len()
    );
    if let Some((k, v)) = header {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(json_body);
    stream.write_all(req.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    split_http(&buf)
}

/// Seed a `config.yaml` in the harness dir with a pinned device identity so the
/// pairing-info route reads a deterministic device_id / name / profile.
fn seed_config(dir: &Path, device_id: &str, name: &str, profile: &str) {
    let yaml = format!("agent:\n  device_id: {device_id}\n  name: {name}\n  profile: {profile}\n");
    std::fs::write(dir.join("config.yaml"), yaml).unwrap();
}

/// The canned snapshot with the FC connected (for the fc_* fields in info).
fn fc_connected_snapshot() -> Value {
    serde_json::json!({
        "fc_connected": true,
        "fc_port": "/dev/ttyACM0",
        "fc_baud": 115200,
    })
}

/// Poll `/api/pairing/info` until the FC triple reflects the canned snapshot.
async fn poll_info_until_fc_connected(socket: &Path) -> Value {
    for _ in 0..100 {
        let (status, body) = unix_get(socket, "/api/pairing/info", None).await;
        if status.contains("200") {
            if let Ok(v) = serde_json::from_str::<Value>(&body) {
                if v.get("fc_connected") == Some(&Value::Bool(true)) {
                    return v;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("pairing info never reflected the connected FC");
}

#[tokio::test]
async fn pairing_info_unpaired_matches_the_golden_shape() {
    let dir = tempfile::tempdir().unwrap();
    seed_config(dir.path(), "abcdef1234567890", "test-drone", "drone");
    // Seed an unpaired pairing.json with a fixed code so pairing_code is stable.
    let h = start_with_state(
        dir.path(),
        Some(r#"{"pairing_code": "ABC234", "code_created_at": 9999999999.0}"#),
        dir.path().join("absent-state.sock"),
    )
    .await;

    let (status, body) = unix_get(&h.socket, "/api/pairing/info", None).await;
    assert!(
        status.contains("200"),
        "pairing info must be 200, was {status}"
    );
    let got: Value = serde_json::from_str(&body).unwrap_or_else(|_| panic!("body: {body}"));
    let want = fixture("pairing_info_unpaired.json");

    // R1: the exact 19-field key set, no field omitted.
    assert_same_keys(&got, &want, "/api/pairing/info (unpaired)");
    assert_eq!(
        got.as_object().unwrap().len(),
        19,
        "must emit all 19 fields"
    );
    // Same shape: every value's kind matches the golden fixture, so a field that
    // is null in the fixture is null here (not omitted, not a different type).
    assert_same_shape(&got, &want, "/api/pairing/info (unpaired)");

    // The seven nullable fields are provably JSON null on a degraded/unpaired
    // agent (not omitted) — the Rule-39 invariant the GCS depends on.
    for k in [
        "bind_state",
        "radio",
        "owner_id",
        "paired_at",
        "radio_peer_device_id",
        "fc_port",
        "fc_baud",
    ] {
        assert_eq!(got[k], Value::Null, "{k} must serialize as JSON null");
    }

    // Static scalars the native surface CAN match byte-for-byte.
    assert_eq!(got["device_id"], want["device_id"]);
    assert_eq!(got["name"], want["name"]);
    assert_eq!(got["mdns_host"], want["mdns_host"], "mdns_host format");
    assert_eq!(got["profile"], want["profile"]);
    assert_eq!(got["role"], want["role"]); // null for a drone
    assert_eq!(got["runtime_mode"], serde_json::json!("packaged"));
    assert_eq!(got["paired"], Value::Bool(false));
    assert_eq!(got["radio_paired"], Value::Bool(false));
    assert_eq!(got["pairing_code"], serde_json::json!("ABC234"));
    assert_eq!(got["fc_connected"], Value::Bool(false));
    // version is a non-empty string (differs from the fixture's capture-time pin).
    assert!(got["version"].is_string() && !got["version"].as_str().unwrap().is_empty());
    // board is a non-empty string; its VALUE differs (native reports "unknown",
    // no HAL-detect port — a documented gap).
    assert!(got["board"].is_string() && !got["board"].as_str().unwrap().is_empty());
    assert_eq!(got["board"], serde_json::json!("unknown"));

    h.stop().await;
}

#[tokio::test]
async fn pairing_info_paired_matches_the_golden_shape() {
    let dir = tempfile::tempdir().unwrap();
    seed_config(dir.path(), "abcdef1234567890", "test-drone", "drone");
    let h = start_with_state(
        dir.path(),
        Some(r#"{"paired": true, "api_key": "ados_K", "owner_id": "user-42", "paired_at": 1700000000.0}"#),
        dir.path().join("absent-state.sock"),
    )
    .await;

    let (status, body) = unix_get(&h.socket, "/api/pairing/info", None).await;
    assert!(
        status.contains("200"),
        "paired info must be 200, was {status}"
    );
    let got: Value = serde_json::from_str(&body).unwrap();
    let want = fixture("pairing_info_paired.json");

    assert_same_keys(&got, &want, "/api/pairing/info (paired)");
    assert_same_shape(&got, &want, "/api/pairing/info (paired)");
    // Paired → owner/paired_at populated, code null.
    assert_eq!(got["paired"], Value::Bool(true));
    assert_eq!(got["owner_id"], serde_json::json!("user-42"));
    assert_eq!(got["paired_at"], serde_json::json!(1700000000.0));
    assert_eq!(got["pairing_code"], Value::Null, "paired → no code in info");

    h.stop().await;
}

#[tokio::test]
async fn pairing_info_reads_the_fc_triple_radio_peer_and_bind_state() {
    let dir = tempfile::tempdir().unwrap();
    seed_config(dir.path(), "abcdef1234567890", "test-drone", "drone");
    // Append the radio peer to the seeded config.
    std::fs::write(
        dir.path().join("config.yaml"),
        "agent:\n  device_id: abcdef1234567890\n  name: test-drone\n  profile: drone\nvideo:\n  wfb:\n    paired_with_device_id: peer-9876543210\n",
    )
    .unwrap();
    // Seed a wfb key (radio_paired) + a bind-state sentinel.
    std::fs::create_dir_all(dir.path().join("wfb")).unwrap();
    std::fs::write(dir.path().join("wfb").join("tx.key"), b"k").unwrap();
    std::fs::write(
        dir.path().join("bind-state.json"),
        r#"{"state":"binding","phase":"key_transfer","active":true,"error":null,"finished_at":12.0,"fingerprint":"ab"}"#,
    )
    .unwrap();

    let mock = MockStateServer::start(dir.path(), fc_connected_snapshot(), Wire::V1Json).await;
    let h = start_with_state(dir.path(), None, mock.path.clone()).await;

    let got = poll_info_until_fc_connected(&h.socket).await;
    // FC triple from the live snapshot.
    assert_eq!(got["fc_connected"], Value::Bool(true));
    assert_eq!(got["fc_port"], serde_json::json!("/dev/ttyACM0"));
    assert_eq!(got["fc_baud"], serde_json::json!(115200));
    // Radio peer + radio_paired from config + the wfb key file.
    assert_eq!(
        got["radio_peer_device_id"],
        serde_json::json!("peer-9876543210")
    );
    assert_eq!(got["radio_paired"], Value::Bool(true));
    // bind_state folds the six fields.
    let bs = got["bind_state"].as_object().expect("bind_state object");
    assert_eq!(bs["state"], serde_json::json!("binding"));
    assert_eq!(bs["active"], Value::Bool(true));
    // The full key set still matches the golden fixture (19 fields).
    assert_same_keys(
        &got,
        &fixture("pairing_info_unpaired.json"),
        "/api/pairing/info (rich)",
    );

    h.stop().await;
    mock.stop().await;
}

#[tokio::test]
async fn pairing_info_ground_station_reports_the_profile_and_role() {
    let dir = tempfile::tempdir().unwrap();
    seed_config(dir.path(), "abcdef1234567890", "gs-node", "ground_station");
    // A ground-station agent reads its role from the mesh role sentinel. The daemon
    // resolves it via the threaded `mesh_role_path` (`<dir>/mesh-role`, wired in
    // `start_full`), so the test seeds the file rather than mutating the process
    // environment.
    std::fs::write(dir.path().join("mesh-role"), "relay\n").unwrap();

    let h = start_with_state(
        dir.path(),
        Some(r#"{"pairing_code": "ABC234", "code_created_at": 9999999999.0}"#),
        dir.path().join("absent-state.sock"),
    )
    .await;

    let (status, body) = unix_get(&h.socket, "/api/pairing/info", None).await;
    assert!(status.contains("200"), "{status}");
    let got: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(got["profile"], serde_json::json!("ground-station"));
    assert_eq!(got["role"], serde_json::json!("relay"));

    h.stop().await;
}

#[tokio::test]
async fn pairing_code_returns_the_code_unpaired_and_409s_paired() {
    let dir = tempfile::tempdir().unwrap();
    // Unpaired with a seeded code.
    let h = start(
        dir.path(),
        Some(r#"{"pairing_code": "ABC234", "code_created_at": 9999999999.0}"#),
    )
    .await;
    let (status, body) = unix_get(&h.socket, "/api/pairing/code", None).await;
    assert!(status.contains("200"), "{status}");
    let got: Value = serde_json::from_str(&body).unwrap();
    let want = fixture("pairing_code.json");
    assert_same_keys(&got, &want, "/api/pairing/code");
    assert_eq!(got["code"], serde_json::json!("ABC234"));
    h.stop().await;

    // Paired → 409 with the {"detail"} body.
    let dir2 = tempfile::tempdir().unwrap();
    let h2 = start(dir2.path(), Some(r#"{"paired": true, "api_key": "k"}"#)).await;
    let (status2, body2) = unix_get(&h2.socket, "/api/pairing/code", None).await;
    assert!(
        status2.contains("409"),
        "paired code must be 409: {status2}"
    );
    let j: Value = serde_json::from_str(&body2).unwrap();
    assert_eq!(j["detail"], serde_json::json!("Already paired"));
    assert!(j.get("error").is_none(), "must be the {{detail}} shape");
    h2.stop().await;
}

#[tokio::test]
async fn pairing_claim_writes_pairing_json_like_the_pairing_manager() {
    let dir = tempfile::tempdir().unwrap();
    seed_config(dir.path(), "abcdef1234567890", "test-drone", "drone");
    // Unpaired, with a pending key the claim must prefer (PairingManager.claim).
    let h = start(
        dir.path(),
        Some(r#"{"pairing_code": "ABC234", "code_created_at": 1.0, "pending_api_key": "ados_PENDING"}"#),
    )
    .await;

    let (status, body) = unix_post(
        &h.socket,
        "/api/pairing/claim",
        None,
        r#"{"user_id":"user-7"}"#,
    )
    .await;
    assert!(status.contains("200"), "claim must be 200, was {status}");
    let got: Value = serde_json::from_str(&body).unwrap();
    let want = fixture("pairing_claim_response.json");

    // The 4-key response shape.
    assert_same_keys(&got, &want, "/api/pairing/claim");
    assert_eq!(got["device_id"], want["device_id"]);
    assert_eq!(got["name"], want["name"]);
    assert_eq!(got["mdns_host"], want["mdns_host"]);
    // The pending key is preferred verbatim (the PairingManager contract).
    assert_eq!(got["api_key"], serde_json::json!("ados_PENDING"));

    // The pairing.json AFTER the write matches PairingManager.claim: exactly the
    // four keys, code + pending dropped.
    let pairing_path = dir.path().join("pairing.json");
    let on_disk: Value =
        serde_json::from_str(&std::fs::read_to_string(&pairing_path).unwrap()).unwrap();
    let keys: std::collections::BTreeSet<_> =
        on_disk.as_object().unwrap().keys().cloned().collect();
    let want_keys: std::collections::BTreeSet<_> = ["paired", "api_key", "owner_id", "paired_at"]
        .into_iter()
        .map(String::from)
        .collect();
    assert_eq!(
        keys, want_keys,
        "claimed pairing.json keys must equal PairingManager.claim"
    );
    assert_eq!(on_disk["paired"], Value::Bool(true));
    assert_eq!(on_disk["api_key"], serde_json::json!("ados_PENDING"));
    assert_eq!(on_disk["owner_id"], serde_json::json!("user-7"));
    assert!(on_disk["paired_at"].is_number());
    assert!(on_disk.get("pairing_code").is_none());
    assert!(on_disk.get("pending_api_key").is_none());

    // A second claim is now a 409.
    let (status2, body2) =
        unix_post(&h.socket, "/api/pairing/claim", None, r#"{"user_id":"x"}"#).await;
    assert!(status2.contains("409"), "re-claim must be 409: {status2}");
    let j: Value = serde_json::from_str(&body2).unwrap();
    assert_eq!(
        j["detail"],
        serde_json::json!("Already paired. Unpair first.")
    );

    h.stop().await;
}

#[tokio::test]
async fn pairing_unpair_clears_pairing_json_and_mints_a_code() {
    let dir = tempfile::tempdir().unwrap();
    seed_config(dir.path(), "abcdef1234567890", "test-drone", "drone");
    // Paired with a key; unpair is gated, so present the key (over the unix
    // socket the on-box trust would bypass it anyway, but presenting it proves
    // the keyed path works end to end).
    let h = start(
        dir.path(),
        Some(r#"{"paired": true, "api_key": "ados_K", "owner_id": "u", "paired_at": 1.0}"#),
    )
    .await;

    let (status, body) = unix_post(
        &h.socket,
        "/api/pairing/unpair",
        Some(("X-ADOS-Key", "ados_K")),
        "",
    )
    .await;
    assert!(status.contains("200"), "unpair must be 200, was {status}");
    let got: Value = serde_json::from_str(&body).unwrap();
    let want = fixture("pairing_unpair_response.json");
    assert_same_keys(&got, &want, "/api/pairing/unpair");
    assert_eq!(got["status"], serde_json::json!("unpaired"));
    // new_code is a fresh 6-char code (random per call).
    let new_code = got["new_code"].as_str().expect("new_code string");
    assert_eq!(new_code.len(), 6, "new_code must be 6 chars: {new_code}");

    // The pairing.json now carries only the fresh code state (paired cleared) —
    // the PairingManager.unpair → get_or_create_code sequence.
    let pairing_path = dir.path().join("pairing.json");
    let on_disk: Value =
        serde_json::from_str(&std::fs::read_to_string(&pairing_path).unwrap()).unwrap();
    assert!(
        on_disk.get("paired").is_none() || on_disk["paired"] == Value::Bool(false),
        "unpaired pairing.json must not be paired: {on_disk}"
    );
    assert!(
        on_disk.get("api_key").is_none(),
        "unpaired must drop the api_key"
    );
    assert_eq!(on_disk["pairing_code"], serde_json::json!(new_code));

    h.stop().await;
}

#[tokio::test]
async fn pairing_unpair_is_409_when_not_paired() {
    let dir = tempfile::tempdir().unwrap();
    // No pairing file → unpaired.
    let h = start(dir.path(), None).await;
    let (status, body) = unix_post(&h.socket, "/api/pairing/unpair", None, "").await;
    assert!(
        status.contains("409"),
        "unpair while unpaired must be 409: {status}"
    );
    let j: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(j["detail"], serde_json::json!("Not paired"));
    assert!(j.get("error").is_none());
    h.stop().await;
}

#[tokio::test]
async fn pairing_unpair_is_gated_by_the_key_when_paired_over_tcp() {
    let dir = tempfile::tempdir().unwrap();
    // Paired: unpair is NOT in the public set, so a keyless TCP request (with a
    // forwarding header to drop the loopback shortcut) must be rejected at the
    // gate, never reaching the handler's not-paired check.
    let h = start(dir.path(), Some(r#"{"paired": true, "api_key": "ados_K"}"#)).await;
    // tcp POST without a key, with a forwarding header.
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", h.port))
        .await
        .unwrap();
    let req = "POST /api/pairing/unpair HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 0\r\nX-Forwarded-For: 203.0.113.7\r\n\r\n";
    stream.write_all(req.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let (status, _b) = split_http(&buf);
    assert!(
        status.contains("401"),
        "keyless unpair must be gated: {status}"
    );
    h.stop().await;
}

// --- command parity (R2: the MAVLink bytes / R3: target_system) ---

use ados_protocol::frame::{decode_len, HEADER_SIZE, MAVLINK_MAX_FRAME};
use ados_protocol::mavlink::ardupilotmega::{MavCmd, MavMessage, COMMAND_LONG_DATA};
use std::sync::Arc;
use tokio::sync::Mutex;

/// A mock MAVLink command socket. Accepts one connection, reads exactly one
/// length-prefixed frame, decodes it with `parse_v2`, and stores the decoded
/// `COMMAND_LONG` for the test to assert. Mirrors the real router's read side
/// (which forwards a client-written frame to the FC).
struct MockMavlinkServer {
    path: PathBuf,
    received: Arc<Mutex<Option<COMMAND_LONG_DATA>>>,
    stop: Option<oneshot::Sender<()>>,
    join: tokio::task::JoinHandle<()>,
}

impl MockMavlinkServer {
    async fn start(dir: &Path) -> Self {
        let path = dir.join("mavlink.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let received: Arc<Mutex<Option<COMMAND_LONG_DATA>>> = Arc::new(Mutex::new(None));
        let received_w = Arc::clone(&received);
        let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
        let join = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut stop_rx => return,
                    accepted = listener.accept() => {
                        if let Ok((mut conn, _addr)) = accepted {
                            // Read one framed command: 4-byte length, then the body.
                            let mut header = [0u8; HEADER_SIZE];
                            if conn.read_exact(&mut header).await.is_err() {
                                continue;
                            }
                            let len = match decode_len(header, MAVLINK_MAX_FRAME, false) {
                                Ok(n) => n,
                                Err(_) => continue,
                            };
                            let mut body = vec![0u8; len];
                            if conn.read_exact(&mut body).await.is_err() {
                                continue;
                            }
                            if let Ok((_h, MavMessage::COMMAND_LONG(d))) =
                                ados_protocol::mavlink::parse_v2(&body)
                            {
                                *received_w.lock().await = Some(d);
                            }
                            // Hold the connection open so the client's lazy
                            // connection stays valid for the assertion window.
                            tokio::spawn(async move {
                                let mut sink = [0u8; 64];
                                loop {
                                    match conn.read(&mut sink).await {
                                        Ok(0) | Err(_) => return,
                                        Ok(_) => {}
                                    }
                                }
                            });
                        }
                    }
                }
            }
        });
        Self {
            path,
            received,
            stop: Some(stop_tx),
            join,
        }
    }

    /// Wait until a command frame has been received + decoded, returning it.
    async fn await_command(&self) -> COMMAND_LONG_DATA {
        for _ in 0..100 {
            if let Some(d) = self.received.lock().await.clone() {
                return d;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("mock mavlink server never received a command frame");
    }

    async fn stop(mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        let _ = self.join.await;
    }
}

/// The canned snapshot the command tests use: FC connected, so the route passes
/// its `fc_connected` gate.
fn fc_up_snapshot() -> Value {
    serde_json::json!({
        "fc_connected": true,
        "fc_port": "/dev/ttyACM0",
        "fc_baud": 115200,
    })
}

/// POST a command and return (status_line, body) over the unix socket.
async fn post_command(socket: &Path, json_body: &str) -> (String, String) {
    unix_post(socket, "/api/command", None, json_body).await
}

/// Bring up the surface with an FC-connected state snapshot AND a live mock
/// MAVLink socket. Returns the harness + the mock so a command test can assert
/// the exact frame the route wrote.
async fn start_with_fc_and_mavlink(dir: &Path) -> (Harness, MockStateServer, MockMavlinkServer) {
    let state_mock = MockStateServer::start(dir, fc_up_snapshot(), Wire::V1Json).await;
    let mav_mock = MockMavlinkServer::start(dir).await;
    let h = start_full(dir, None, state_mock.path.clone(), mav_mock.path.clone()).await;
    // Wait for the state client to read fc_connected=true so the command gate
    // passes deterministically.
    poll_info_until_fc_connected(&h.socket).await;
    (h, state_mock, mav_mock)
}

#[tokio::test]
async fn command_arm_writes_a_component_arm_disarm_frame() {
    let dir = tempfile::tempdir().unwrap();
    let (h, state_mock, mav_mock) = start_with_fc_and_mavlink(dir.path()).await;

    let (status, body) = post_command(&h.socket, r#"{"cmd":"arm"}"#).await;
    assert!(status.contains("200"), "arm must be 200, was {status}");
    let got: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(got["status"], serde_json::json!("ok"));
    assert_eq!(got["cmd"], serde_json::json!("arm"));

    let d = mav_mock.await_command().await;
    assert_eq!(d.command, MavCmd::MAV_CMD_COMPONENT_ARM_DISARM);
    assert_eq!(d.param1, 1.0);
    // R3: single-vehicle target.
    assert_eq!(d.target_system, 1);
    assert_eq!(d.target_component, 1);

    h.stop().await;
    state_mock.stop().await;
    mav_mock.stop().await;
}

#[tokio::test]
async fn command_disarm_writes_param1_zero() {
    let dir = tempfile::tempdir().unwrap();
    let (h, state_mock, mav_mock) = start_with_fc_and_mavlink(dir.path()).await;

    let (status, _b) = post_command(&h.socket, r#"{"cmd":"disarm"}"#).await;
    assert!(status.contains("200"), "{status}");
    let d = mav_mock.await_command().await;
    assert_eq!(d.command, MavCmd::MAV_CMD_COMPONENT_ARM_DISARM);
    assert_eq!(d.param1, 0.0);

    h.stop().await;
    state_mock.stop().await;
    mav_mock.stop().await;
}

#[tokio::test]
async fn command_takeoff_default_altitude_is_ten_in_param7() {
    let dir = tempfile::tempdir().unwrap();
    let (h, state_mock, mav_mock) = start_with_fc_and_mavlink(dir.path()).await;

    let (status, body) = post_command(&h.socket, r#"{"cmd":"takeoff"}"#).await;
    assert!(status.contains("200"), "{status}");
    let got: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(got["altitude"], serde_json::json!(10.0));
    let d = mav_mock.await_command().await;
    assert_eq!(d.command, MavCmd::MAV_CMD_NAV_TAKEOFF);
    assert_eq!(d.param7, 10.0);

    h.stop().await;
    state_mock.stop().await;
    mav_mock.stop().await;
}

#[tokio::test]
async fn command_takeoff_reads_the_altitude_arg() {
    let dir = tempfile::tempdir().unwrap();
    let (h, state_mock, mav_mock) = start_with_fc_and_mavlink(dir.path()).await;

    let (status, _b) = post_command(&h.socket, r#"{"cmd":"takeoff","args":[25]}"#).await;
    assert!(status.contains("200"), "{status}");
    let d = mav_mock.await_command().await;
    assert_eq!(d.param7, 25.0);

    h.stop().await;
    state_mock.stop().await;
    mav_mock.stop().await;
}

#[tokio::test]
async fn command_land_writes_nav_land_all_zero() {
    let dir = tempfile::tempdir().unwrap();
    let (h, state_mock, mav_mock) = start_with_fc_and_mavlink(dir.path()).await;

    let (status, _b) = post_command(&h.socket, r#"{"cmd":"land"}"#).await;
    assert!(status.contains("200"), "{status}");
    let d = mav_mock.await_command().await;
    assert_eq!(d.command, MavCmd::MAV_CMD_NAV_LAND);
    for p in [
        d.param1, d.param2, d.param3, d.param4, d.param5, d.param6, d.param7,
    ] {
        assert_eq!(p, 0.0);
    }

    h.stop().await;
    state_mock.stop().await;
    mav_mock.stop().await;
}

#[tokio::test]
async fn command_rtl_writes_do_set_mode_param2_six() {
    let dir = tempfile::tempdir().unwrap();
    let (h, state_mock, mav_mock) = start_with_fc_and_mavlink(dir.path()).await;

    let (status, _b) = post_command(&h.socket, r#"{"cmd":"rtl"}"#).await;
    assert!(status.contains("200"), "{status}");
    let d = mav_mock.await_command().await;
    // The `rtl` shortcut commands Return-to-Launch: DO_SET_MODE p1=1, p2=6 (RTL).
    assert_eq!(d.command, MavCmd::MAV_CMD_DO_SET_MODE);
    assert_eq!(d.param1, 1.0);
    assert_eq!(d.param2, 6.0);

    h.stop().await;
    state_mock.stop().await;
    mav_mock.stop().await;
}

#[tokio::test]
async fn command_mode_rtl_writes_do_set_mode_param2_six() {
    let dir = tempfile::tempdir().unwrap();
    let (h, state_mock, mav_mock) = start_with_fc_and_mavlink(dir.path()).await;

    let (status, body) = post_command(&h.socket, r#"{"cmd":"mode","args":["RTL"]}"#).await;
    assert!(status.contains("200"), "{status}");
    let got: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(got["mode"], serde_json::json!("RTL"));
    let d = mav_mock.await_command().await;
    assert_eq!(d.command, MavCmd::MAV_CMD_DO_SET_MODE);
    assert_eq!(d.param1, 1.0);
    assert_eq!(d.param2, 6.0);

    h.stop().await;
    state_mock.stop().await;
    mav_mock.stop().await;
}

#[tokio::test]
async fn command_503_when_fc_not_connected() {
    let dir = tempfile::tempdir().unwrap();
    // FC-down snapshot → the command gate fails before any send. The MAVLink
    // socket is live, proving the 503 is the FC gate, not a send failure.
    let state_mock = MockStateServer::start(
        dir.path(),
        serde_json::json!({"fc_connected": false}),
        Wire::V1Json,
    )
    .await;
    let mav_mock = MockMavlinkServer::start(dir.path()).await;
    let h = start_full(
        dir.path(),
        None,
        state_mock.path.clone(),
        mav_mock.path.clone(),
    )
    .await;
    // Let the state client read the (false) snapshot.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let (status, body) = post_command(&h.socket, r#"{"cmd":"arm"}"#).await;
    assert!(
        status.contains("503"),
        "FC-down command must be 503: {status}"
    );
    let j: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(j["detail"], serde_json::json!("FC not connected"));
    assert!(j.get("error").is_none(), "must be the {{detail}} shape");

    h.stop().await;
    state_mock.stop().await;
    mav_mock.stop().await;
}

#[tokio::test]
async fn command_503_when_the_mavlink_socket_is_absent() {
    let dir = tempfile::tempdir().unwrap();
    // FC connected (so the gate passes) but NO MAVLink socket → the send fails →
    // 503 "No MAVLink connection".
    let state_mock = MockStateServer::start(dir.path(), fc_up_snapshot(), Wire::V1Json).await;
    let h = start_full(
        dir.path(),
        None,
        state_mock.path.clone(),
        dir.path().join("absent-mavlink.sock"),
    )
    .await;
    poll_info_until_fc_connected(&h.socket).await;

    let (status, body) = post_command(&h.socket, r#"{"cmd":"arm"}"#).await;
    assert!(
        status.contains("503"),
        "an absent mavlink socket must 503: {status}"
    );
    let j: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(j["detail"], serde_json::json!("No MAVLink connection"));

    h.stop().await;
    state_mock.stop().await;
}

#[tokio::test]
async fn command_400_on_unknown_command() {
    let dir = tempfile::tempdir().unwrap();
    let (h, state_mock, mav_mock) = start_with_fc_and_mavlink(dir.path()).await;

    let (status, body) = post_command(&h.socket, r#"{"cmd":"warp-speed"}"#).await;
    assert!(
        status.contains("400"),
        "unknown command must be 400: {status}"
    );
    let j: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        j["detail"],
        serde_json::json!("Unknown command: warp-speed")
    );

    h.stop().await;
    state_mock.stop().await;
    mav_mock.stop().await;
}

#[tokio::test]
async fn command_400_on_unknown_mode_and_missing_mode_name() {
    let dir = tempfile::tempdir().unwrap();
    let (h, state_mock, mav_mock) = start_with_fc_and_mavlink(dir.path()).await;

    // Unknown mode name.
    let (status, body) = post_command(&h.socket, r#"{"cmd":"mode","args":["NOPE"]}"#).await;
    assert!(status.contains("400"), "unknown mode must be 400: {status}");
    let j: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(j["detail"], serde_json::json!("Unknown mode: NOPE"));

    // Missing mode name.
    let (status2, body2) = post_command(&h.socket, r#"{"cmd":"mode"}"#).await;
    assert!(
        status2.contains("400"),
        "missing mode name must be 400: {status2}"
    );
    let j2: Value = serde_json::from_str(&body2).unwrap();
    assert_eq!(j2["detail"], serde_json::json!("Mode name required"));

    h.stop().await;
    state_mock.stop().await;
    mav_mock.stop().await;
}

#[tokio::test]
async fn commands_catalog_matches_the_simple_commands() {
    let dir = tempfile::tempdir().unwrap();
    let h = start(dir.path(), None).await;
    let (status, body) = unix_get(&h.socket, "/api/commands", None).await;
    assert!(status.contains("200"), "{status}");
    let got: Value = serde_json::from_str(&body).unwrap();
    let commands = got["commands"].as_object().expect("commands object");
    // The SIMPLE_COMMANDS catalog, verbatim, with their descriptions: the six
    // original text commands plus the three agent-native fleet-board actions.
    let want = serde_json::json!({
        "arm": "Arm the vehicle",
        "disarm": "Disarm the vehicle",
        "takeoff": "Takeoff to altitude (args: [altitude_m])",
        "land": "Land at current position",
        "rtl": "Return to launch",
        "mode": "Set flight mode (args: [mode_name])",
        "killSwitch": "Emergency motor cut: force-disarm now, bypassing in-flight \
                       safety checks. Stops the motors immediately, so an airborne \
                       vehicle drops.",
        "pauseMission": "Pause the current mission / auto flight (hold position)",
        "resumeMission": "Resume a paused mission / auto flight",
    });
    assert_eq!(
        &Value::Object(commands.clone()),
        &want,
        "the catalog must match SIMPLE_COMMANDS verbatim"
    );
    h.stop().await;
}

#[tokio::test]
async fn command_is_gated_by_the_key_when_paired_over_tcp() {
    let dir = tempfile::tempdir().unwrap();
    // /api/command is NOT public, so a keyless TCP request (with a forwarding
    // header to drop the loopback shortcut) is rejected at the gate.
    let h = start(dir.path(), Some(r#"{"paired": true, "api_key": "ados_K"}"#)).await;
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", h.port))
        .await
        .unwrap();
    let body = r#"{"cmd":"arm"}"#;
    let req = format!(
        "POST /api/command HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\nX-Forwarded-For: 203.0.113.7\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let (status, _b) = split_http(&buf);
    assert!(
        status.contains("401"),
        "keyless command must be gated: {status}"
    );
    h.stop().await;
}
