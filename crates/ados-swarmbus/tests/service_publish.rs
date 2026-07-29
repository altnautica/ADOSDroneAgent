//! A live run of the daemon: does it actually bind its socket and publish?
//!
//! The radio is Linux-only and needs `CAP_NET_RAW`, so no beacon is transmitted or
//! received here. That is deliberately not what this test is for. It proves the parts
//! of the service that have nothing to do with the radio and everything to do with
//! whether the service is usable at all:
//!
//! - It binds `swarm.sock` and publishes **before and without** a working radio, so a
//!   node whose adapter has not come up still answers `GET /api/swarm/neighbors`
//!   instead of leaving the operator's fleet view with no socket to read.
//! - It publishes at the beacon rate, repeatedly, not once at startup.
//! - It replays the last table to a late subscriber, so a consumer that connects
//!   between publishes is not blind for half a second.
//! - It reports the real fleet identity, and zeroed counters rather than fabricated
//!   activity.
//! - It removes its socket on shutdown, so a restart does not inherit a stale one.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ados_protocol::state::encode_v2;
use ados_swarmbus::publish::{COUNTER_KEYS, NEIGHBOR_KEYS};
use ados_swarmbus::SwarmBusConfig;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Notify;

/// A ground-station config rooted in `dir`: slot 0, so the transmit loop is correctly
/// never started and the identity gate passes.
fn ground_station_config(dir: &Path) -> SwarmBusConfig {
    SwarmBusConfig {
        profile: Some("ground_station".to_string()),
        device_id: "ados-gs-test".to_string(),
        // A name no host has, so the radio open fails the way it does on a node whose
        // adapter is not up yet — which is the condition under test.
        interface: "nonexistent-swarm-iface0".to_string(),
        fleet_id: 7,
        fleet_slot: 0,
        socket_dir: dir.to_string_lossy().into_owned(),
    }
}

/// Serve one vehicle-state snapshot on `state.sock`, the way the MAVLink router does.
fn serve_state(path: std::path::PathBuf, snapshot: Value) {
    let listener = UnixListener::bind(&path).unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let frame = encode_v2(&snapshot).unwrap();
            tokio::spawn(async move {
                let _ = stream.write_all(&frame).await;
                let _ = stream.flush().await;
                tokio::time::sleep(Duration::from_secs(30)).await;
            });
        }
    });
}

/// Connect to the swarm socket and read one published line, retrying the connect
/// until the service has bound it.
async fn read_one_line(path: &Path) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let stream = loop {
        match UnixStream::connect(path).await {
            Ok(s) => break s,
            Err(e) if tokio::time::Instant::now() < deadline => {
                let _ = e;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(e) => panic!("connect {}: {e}", path.display()),
        }
    };
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
        .await
        .expect("a publish arrived within 5 s")
        .expect("the line read cleanly");
    serde_json::from_str(&line).unwrap_or_else(|e| panic!("publish {line:?}: {e}"))
}

#[tokio::test]
async fn the_service_publishes_the_contract_without_a_working_radio() {
    let dir = tempfile::tempdir().unwrap();
    serve_state(
        dir.path().join("state.sock"),
        json!({
            "armed": true,
            "mode": "GUIDED",
            "position": {"lat": 12.9716, "lon": 77.5946, "alt_rel": 32.5},
            "velocity": {"vx": 1.2, "vy": -0.4, "vz": 0.0},
            "gps": {"fix_type": 3},
        }),
    );

    let cancel = Arc::new(Notify::new());
    let cfg = ground_station_config(dir.path());
    let run_cancel = cancel.clone();
    let service = tokio::spawn(async move { ados_swarmbus::run(cfg, run_cancel).await });

    let swarm_sock = dir.path().join("swarm.sock");
    let first = read_one_line(&swarm_sock).await;

    // The real fleet identity, not a guess: this is a RUNNING bus, so unlike the
    // absent-service case it must report its actual fleet and slot.
    assert_eq!(first["fleet_id"], json!(7));
    assert_eq!(first["slot"], json!(0), "the ground station is slot 0");
    // No radio, so no neighbours and no transmissions — and emphatically no
    // fabricated ones.
    assert_eq!(first["neighbors"], json!([]));
    for k in COUNTER_KEYS {
        assert_eq!(first["counters"][k], json!(0), "{k} must be zero");
    }
    // The contract's key sets are present and complete even with an empty table.
    let keys: Vec<&str> = first
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        vec!["counters", "fleet_id", "neighbors", "slot", "slots"]
    );
    for k in COUNTER_KEYS {
        assert!(first["counters"].get(k).is_some(), "counters missing {k}");
    }
    // The row-key contract is exported for a consumer to conform against.
    assert_eq!(NEIGHBOR_KEYS.len(), 18);

    // It publishes REPEATEDLY at the beacon rate, not once at startup. A second
    // reader gets the replayed last table immediately, then a fresh one.
    let stream = UnixStream::connect(&swarm_sock)
        .await
        .expect("late connect");
    let mut reader = BufReader::new(stream);
    let mut lines = 0;
    let started = std::time::Instant::now();
    while lines < 3 && started.elapsed() < Duration::from_secs(5) {
        let mut line = String::new();
        match tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line)).await {
            Ok(Ok(n)) if n > 0 => {
                let v: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(v["fleet_id"], json!(7), "every publish carries the fleet");
                lines += 1;
            }
            _ => break,
        }
    }
    assert!(
        lines >= 3,
        "only {lines} publishes in {:?}; the publish loop is not running at the beacon rate",
        started.elapsed()
    );
    // Three lines including the replay must arrive inside two beacon periods plus
    // jitter; far longer would mean the cadence is wrong.
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "three publishes took {:?}",
        started.elapsed()
    );

    // Shutdown removes the socket, so a restart cannot inherit a stale one.
    cancel.notify_waiters();
    let _ = tokio::time::timeout(Duration::from_secs(5), service).await;
    assert!(
        !swarm_sock.exists(),
        "the socket must be removed on shutdown"
    );
}

/// A drone on the ground station's slot 0 is the state a fresh box boots in, and it
/// must be caught before anything radiates: a duplicate slot thrashes the wfb-ng FEC
/// decoder about once a second, which presents as unexplained link loss rather than as
/// a configuration error.
#[test]
fn a_drone_left_on_the_ground_slot_is_rejected_before_it_can_radiate() {
    let cfg = SwarmBusConfig::from_yaml("agent:\n  profile: drone\n");
    assert_eq!(cfg.fleet_slot, 0);
    assert!(
        cfg.identity_error().is_some(),
        "a drone on slot 0 must fail the identity gate"
    );
    // And the correctly-provisioned equivalent passes.
    let ok = SwarmBusConfig::from_yaml(
        "agent:\n  profile: drone\nvideo:\n  wfb:\n    fleet_id: 7\n    fleet_slot: 3\n",
    );
    assert_eq!(ok.identity_error(), None);
    assert!(!ok.is_ground_station());
}
