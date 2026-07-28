//! End-to-end proof of the swarm bus, from a peer's on-air bytes to the JSON
//! Mission Control reads.
//!
//! Every other test in this phase covers one link. This one covers the whole chain
//! with nothing stubbed in the middle:
//!
//! 1. Build the exact frame a peer drone puts on the air — radiotap, 802.11 with the
//!    swarm magic and the fleet id, and a ChaCha20-Poly1305-sealed beacon.
//! 2. Feed it through the real receive classification into a real neighbour table.
//! 3. Serialize the real published payload and serve it over a real Unix socket, the
//!    way `ados-swarmbus` does.
//! 4. Bring the native HTTP surface up against that socket and `GET
//!    /api/swarm/neighbors` over HTTP/1.1.
//! 5. Assert the body is the published contract, field by field.
//!
//! If any link in that chain is wrong — a byte offset, an endianness, a key name, a
//! null rule — this fails. The unit tests say each piece is correct; this says the
//! pieces are connected.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ados_control::{run_with_paths, DaemonPaths};
use ados_swarmbus::beacon::{SwarmBeacon, STATUS_ARMED, STATUS_GPS_OK, STATUS_GUIDED, STATUS_HERO};
use ados_swarmbus::crypto::{derive_fleet_key, SwarmCipher};
use ados_swarmbus::frame::{build_frame, SwarmFrameKind};
use ados_swarmbus::ingest::{ingest_frame, Ingest};
use ados_swarmbus::neighbors::NeighborTable;
use ados_swarmbus::publish::{encode_line, neighbors_payload, COUNTER_KEYS, NEIGHBOR_KEYS};
use ados_swarmbus::ModePrecedence;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UnixListener, UnixStream};
use tokio::sync::oneshot;

const FLEET: u16 = 7;
/// The ground station's own slot: it listens, it never beacons.
const OWN_SLOT: u8 = 0;

/// Bring the native surface up against a swarm socket, with everything else pointed
/// at absent paths in the temp dir.
async fn start(dir: &Path, swarm_socket: PathBuf) -> (PathBuf, oneshot::Sender<()>) {
    let socket = dir.join("control.sock");
    let probe = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);

    let paths = DaemonPaths {
        control_socket: socket.clone(),
        control_tcp_port: port,
        pairing_path: dir.join("pairing.json"),
        dashboard_pin_path: dir.join("dashboard-pin.json"),
        mcp_token_path: dir.join("mcp-token.json"),
        state_socket: dir.join("absent-state.sock"),
        swarm_socket,
        mavlink_socket: dir.join("absent-mavlink.sock"),
        config_path: dir.join("config.yaml"),
        wfb_key_dir: dir.join("wfb"),
        bind_state_path: dir.join("bind-state.json"),
        logd_query_socket: dir.join("absent-logd.sock"),
        board_path: dir.join("board.json"),
        params_path: dir.join("params.json"),
        profile_conf_path: dir.join("profile.conf"),
        mesh_role_path: dir.join("mesh-role"),
    };
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    tokio::spawn(run_with_paths(paths, async move {
        let _ = stop_rx.await;
    }));
    for _ in 0..300 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    (socket, stop_tx)
}

/// Minimal HTTP/1.1 GET over the control socket, returning (status line, body).
async fn get(socket: &Path, path: &str) -> (String, String) {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut stream = loop {
        match UnixStream::connect(socket).await {
            Ok(s) => break s,
            Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(10)).await
            }
            Err(e) => panic!("connect {}: {e}", socket.display()),
        }
    };
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    (
        head.lines().next().unwrap_or("").to_string(),
        body.to_string(),
    )
}

/// Serve `line` on a Unix socket forever, the way `IpcBroadcast` replays its last
/// buffer to every client that connects.
fn serve_line(path: PathBuf, line: Vec<u8>) {
    let listener = UnixListener::bind(&path).unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let line = line.clone();
            tokio::spawn(async move {
                let _ = stream.write_all(&line).await;
                let _ = stream.flush().await;
                // Hold the connection so the reader stays in its read loop.
                tokio::time::sleep(Duration::from_secs(30)).await;
            });
        }
    });
}

/// Two peer drones, as they would actually appear on the air. Slot 3 is armed,
/// guided, has a fix and is the hero; slot 9 is disarmed with no fix and its
/// separation layer engaged.
fn peers() -> (SwarmBeacon, SwarmBeacon) {
    let hero = SwarmBeacon {
        slot: 3,
        seq_ms: 41234,
        lat: 129_716_000,
        lon: 775_946_000,
        alt_dm: 325,
        vx_cms: 120,
        vy_cms: -40,
        vz_cms: 0,
        status: STATUS_ARMED | STATUS_GUIDED | STATUS_GPS_OK | STATUS_HERO,
    };
    let mut degraded = SwarmBeacon {
        slot: 9,
        seq_ms: 9001,
        lat: 129_720_000,
        lon: 775_950_000,
        alt_dm: -150,
        vx_cms: 0,
        vy_cms: 0,
        vz_cms: -100,
        status: 0,
    };
    degraded.set_precedence(ModePrecedence::HardSeparation);
    (hero, degraded)
}

/// Build the neighbour table the way the bus does: real frames in, real
/// classification, real table.
fn table_from_the_air(now: Instant) -> NeighborTable {
    let cipher = SwarmCipher::new(&derive_fleet_key(Some(&[7u8; 64])));
    let mut table = NeighborTable::new(OWN_SLOT);
    let (hero, degraded) = peers();

    for (seq, beacon) in [hero, degraded].into_iter().enumerate() {
        let frame = build_frame(
            FLEET,
            seq as u16,
            &cipher.seal(SwarmFrameKind::Beacon, &beacon.encode()),
        );
        // The frame is 87 bytes injected: 13 radiotap + 24 802.11 + 50 payload.
        assert_eq!(frame.len(), 87, "the on-air frame size changed");
        let outcome = ingest_frame(&frame, FLEET, &cipher, &mut table, now);
        assert_eq!(
            outcome,
            Ingest::Beacon(beacon),
            "slot {} did not survive the wire",
            beacon.slot
        );
    }

    // A frame from another fleet on the same channel must not appear in the table.
    let intruder = build_frame(
        FLEET + 1,
        99,
        &cipher.seal(SwarmFrameKind::Beacon, &peers().0.encode()),
    );
    assert!(matches!(
        ingest_frame(&intruder, FLEET, &cipher, &mut table, now),
        Ingest::Rejected(_)
    ));

    // Nor must a forgery under a different fleet key.
    let forger = SwarmCipher::new(&derive_fleet_key(Some(&[8u8; 64])));
    let forged = build_frame(
        FLEET,
        100,
        &forger.seal(SwarmFrameKind::Beacon, &peers().0.encode()),
    );
    assert!(matches!(
        ingest_frame(&forged, FLEET, &cipher, &mut table, now),
        Ingest::Rejected(_)
    ));

    assert_eq!(table.len(), 2, "exactly the two legitimate peers");
    assert_eq!(table.counters().beacons_rx, 2);
    assert_eq!(
        table.counters().beacons_bad_tag,
        1,
        "the forgery was counted"
    );
    table
}

/// The whole chain: peer bytes on the air become the JSON the operator's fleet view
/// reads, over a real socket and a real HTTP request.
#[tokio::test]
async fn a_peers_on_air_beacon_becomes_the_published_http_body() {
    let dir = tempfile::tempdir().unwrap();
    let swarm_sock = dir.path().join("swarm.sock");

    let now = Instant::now();
    let table = table_from_the_air(now);
    let device_ids = std::collections::BTreeMap::from([
        (3u8, "ados-abc123".to_string()),
        (9u8, "ados-def456".to_string()),
    ]);
    // Publish the table exactly as the bus does, 420 ms after the beacons landed.
    let published = neighbors_payload(FLEET, &table, &device_ids, now + Duration::from_millis(420));
    serve_line(swarm_sock.clone(), encode_line(&published));

    let (socket, stop) = start(dir.path(), swarm_sock).await;

    // The reader connects and decodes asynchronously; poll until the table lands.
    let mut body = Value::Null;
    for _ in 0..300 {
        let (status, raw) = get(&socket, "/api/swarm/neighbors").await;
        assert!(status.contains("200"), "must be 200, was {status}");
        body = serde_json::from_str(&raw).unwrap_or_else(|e| panic!("body {raw}: {e}"));
        if body["fleet_id"] != Value::Null {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // The full contract, value for value. This is the body Mission Control's beacon
    // store is typed against.
    assert_eq!(
        body,
        json!({
            "fleet_id": 7,
            "slot": 0,
            "neighbors": [
                {
                    "slot": 3,
                    "device_id": "ados-abc123",
                    "seq_ms": 41234,
                    "lat": 12.9716,
                    "lon": 77.5946,
                    "alt_m": 32.5,
                    "vx_ms": 1.2,
                    "vy_ms": -0.4,
                    "vz_ms": 0.0,
                    "heading_deg": 341.565051177078,
                    "armed": true,
                    "guided": true,
                    "emergency": false,
                    "gps_ok": true,
                    "hero": true,
                    "mode_precedence": "hold",
                    "age_ms": 420,
                    "rssi_dbm": Value::Null,
                },
                {
                    "slot": 9,
                    "device_id": "ados-def456",
                    "seq_ms": 9001,
                    "lat": 12.972,
                    "lon": 77.595,
                    "alt_m": -15.0,
                    "vx_ms": 0.0,
                    "vy_ms": 0.0,
                    "vz_ms": -1.0,
                    "heading_deg": 0.0,
                    "armed": false,
                    "guided": false,
                    "emergency": false,
                    "gps_ok": false,
                    "hero": false,
                    "mode_precedence": "hard-separation",
                    "age_ms": 420,
                    "rssi_dbm": Value::Null,
                },
            ],
            "counters": {
                "beacons_tx": 0,
                "beacons_rx": 2,
                "beacons_bad_magic": 0,
                "beacons_bad_tag": 1,
                "beacons_stale_dropped": 0,
                "neighbors_now": 2,
            },
        }),
        "the published contract drifted"
    );

    // The consolidated route carries the same rows.
    let (status, raw) = get(&socket, "/api/status/full").await;
    assert!(status.contains("200"), "{status}");
    let full: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(
        full["neighbors"], body["neighbors"],
        "/api/status/full must fold in the same rows"
    );

    let _ = stop.send(());
}

/// With no bus running, both surfaces stay honest: the dedicated route answers 200
/// with a structurally-complete, null-identified empty table, and the consolidated
/// route omits the key entirely rather than claiming "no neighbours heard".
#[tokio::test]
async fn an_absent_bus_degrades_without_fabricating_a_fleet() {
    let dir = tempfile::tempdir().unwrap();
    let (socket, stop) = start(dir.path(), dir.path().join("absent-swarm.sock")).await;

    let (status, raw) = get(&socket, "/api/swarm/neighbors").await;
    assert!(status.contains("200"), "must be 200, was {status}");
    let body: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(body["fleet_id"], Value::Null, "a guessed fleet id is a lie");
    assert_eq!(body["slot"], Value::Null);
    assert_eq!(body["neighbors"], json!([]));
    for k in COUNTER_KEYS {
        assert_eq!(
            body["counters"][k],
            json!(0),
            "{k} must be zero, not absent"
        );
    }

    let (status, raw) = get(&socket, "/api/status/full").await;
    assert!(status.contains("200"), "{status}");
    let full: Value = serde_json::from_str(&raw).unwrap();
    assert!(
        full.get("neighbors").is_none(),
        "an absent bus must omit the key, not claim an empty fleet"
    );

    let _ = stop.send(());
}

/// A payload from a skewed agent build must still answer for every key the GCS store
/// requires. A missing key there is a runtime `undefined` in the operator's table, and
/// the version skew that produces it is exactly what happens mid-fleet-upgrade.
#[tokio::test]
async fn a_skewed_producer_payload_is_normalised_to_the_full_contract() {
    let dir = tempfile::tempdir().unwrap();
    let swarm_sock = dir.path().join("swarm.sock");
    // An older producer: no `slot`, one counter, one row.
    let mut line = serde_json::to_vec(&json!({
        "fleet_id": 4,
        "neighbors": [{"slot": 2, "lat": 1.0, "lon": 2.0}],
        "counters": {"beacons_rx": 5},
    }))
    .unwrap();
    line.push(b'\n');
    serve_line(swarm_sock.clone(), line);

    let (socket, stop) = start(dir.path(), swarm_sock).await;

    let mut body = Value::Null;
    for _ in 0..300 {
        let (_, raw) = get(&socket, "/api/swarm/neighbors").await;
        body = serde_json::from_str(&raw).unwrap();
        if body["fleet_id"] != Value::Null {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(body["fleet_id"], json!(4), "carried through");
    assert_eq!(body["slot"], Value::Null, "omitted by the producer");
    for k in COUNTER_KEYS {
        assert!(
            body["counters"].get(k).is_some(),
            "{k} must be filled, not missing"
        );
    }
    assert_eq!(body["counters"]["beacons_rx"], json!(5));
    assert_eq!(body["counters"]["beacons_tx"], json!(0), "filled default");
    // The row is passed through as the producer sent it — this surface never invents
    // beacon fields it did not receive.
    assert_eq!(body["neighbors"][0]["slot"], json!(2));
    assert!(NEIGHBOR_KEYS.contains(&"mode_precedence"));

    let _ = stop.send(());
}
