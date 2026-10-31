use super::*;
use ados_swarm::OWN_STATE_STALE;

/// A run of injection failures ends the transmitter; a single success in the
/// run clears it, so an occasional full queue never tears the radio down.
#[test]
fn only_a_consecutive_run_of_tx_failures_gives_the_radio_up() {
    let mut f = TxFailures::default();
    for _ in 0..TX_FAILURE_LIMIT - 1 {
        assert!(!f.record(false));
    }
    assert!(!f.record(true), "a success clears the run");
    for _ in 0..TX_FAILURE_LIMIT - 1 {
        assert!(!f.record(false));
    }
    assert!(f.record(false), "the limit-th consecutive failure gives up");
}

/// The config pin wins when set, because an operator who names an interface
/// means it.
#[test]
fn a_configured_interface_pin_takes_precedence() {
    let pinned = |iface: &str| SwarmBusConfig {
        interface: iface.to_string(),
        ..SwarmBusConfig::default()
    };
    assert_eq!(
        resolve_interface(&pinned("wlan9")).as_deref(),
        Some("wlan9")
    );
    // Whitespace is trimmed rather than passed to the kernel as a name.
    assert_eq!(
        resolve_interface(&pinned("  wlan9  ")).as_deref(),
        Some("wlan9")
    );
}

/// An empty pin must fall through to the sidecar, not resolve to `""` — an empty
/// interface name would open a socket bound to nothing and produce a bus nobody
/// hears, with no error anywhere.
#[test]
fn an_empty_pin_falls_through_rather_than_resolving_to_an_empty_name() {
    let cfg = SwarmBusConfig::default();
    assert_eq!(cfg.interface, "", "the shipped default is empty");
    // With no sidecar on this host the resolution is None, never Some("").
    assert_ne!(resolve_interface(&cfg).as_deref(), Some(""));
    let blank = SwarmBusConfig {
        interface: "   ".to_string(),
        ..SwarmBusConfig::default()
    };
    assert_ne!(resolve_interface(&blank).as_deref(), Some(""));
}

#[test]
fn the_sidecar_supplies_the_live_interface_and_degrades_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wfb-stats.json");

    assert_eq!(interface_from_sidecar(&path), None, "absent file");

    std::fs::write(&path, r#"{"state":"linked","interface":"wlan1"}"#).unwrap();
    assert_eq!(interface_from_sidecar(&path).as_deref(), Some("wlan1"));

    // The radio manager writes an empty interface before it has selected an
    // adapter; that is "not yet", not a name.
    std::fs::write(&path, r#"{"state":"scanning","interface":""}"#).unwrap();
    assert_eq!(interface_from_sidecar(&path), None);

    for bad in [
        r#"{"state":"linked"}"#,
        r#"{"interface":null}"#,
        r#"{"interface":42}"#,
        "not json",
        "",
    ] {
        std::fs::write(&path, bad).unwrap();
        assert_eq!(interface_from_sidecar(&path), None, "{bad}");
    }
}

/// The publish cadence must match the beacon rate: publishing slower would make
/// the operator's fleet view lag the aircraft, and faster would burn CPU
/// re-serializing an unchanged table.
#[test]
fn the_publish_cadence_is_the_beacon_rate() {
    assert_eq!(BEACON_PERIOD, Duration::from_millis(500));
    assert!(
        REGISTRY_REFRESH > BEACON_PERIOD * 10,
        "pairing is a human-scale event; do not re-read the registry per publish"
    );
}

/// A snapshot whose producer has stopped must not keep being broadcast as
/// this node's state. Every receiving drone dead-reckons the beacon forward,
/// so a frozen snapshot radiates as continued motion the aircraft is not
/// performing.
#[test]
fn a_stale_snapshot_is_not_broadcast_as_this_nodes_state() {
    let held = StateSnapshot {
        value: serde_json::json!({
            "armed": true,
            "mode": "GUIDED",
            "position": {"lat": 12.34, "lon": 56.78, "alt_rel": 40.0},
            "velocity": {"vx": 8.0, "vy": 0.0, "vz": 0.0},
            // A live fix: the router decoded a POSITION a moment ago.
            "position_age_ms": 40,
        }),
        received: Instant::now(),
    };

    // Fresh: the full body goes out, which is the whole point of the bus.
    let fresh = own_beacon(Some(&held), held.received, 3, 0);
    assert_ne!(fresh.lat, 0);
    assert_ne!(fresh.vx_cms, 0);
    assert!(fresh.armed() && fresh.guided());

    // One window later the producer itself has gone quiet: nothing in the
    // snapshot is current. The node stays on the bus carrying only its slot.
    let stale = own_beacon(Some(&held), held.received + OWN_STATE_STALE, 3, 0);
    assert_eq!((stale.lat, stale.lon, stale.vx_cms), (0, 0, 0));
    assert_eq!(stale.status, 0, "no reading from a dead producer");
    assert_eq!(stale.slot, 3, "the node is still on the bus");
}

/// A node whose GPS has died but whose flight controller keeps heartbeating
/// must not keep radiating its last fix to the formation — and must keep
/// radiating everything else it knows.
///
/// The arrival-age gate cannot see this case: the MAVLink router is alive and
/// publishing on its unconditional cadence, so every snapshot looks fresh.
/// Only the producer-reported fix age distinguishes it. A frozen fix is
/// withheld, because every receiving drone dead-reckons the position and
/// velocity in the beacon FORWARD. The armed, guided, hero, emergency and
/// precedence readings are live, and zeroing them broadcast an armed drone in
/// hard separation as a disarmed one on `hold`.
#[test]
fn a_frozen_fix_under_a_live_producer_withholds_only_the_position() {
    use ados_swarm::ModePrecedence;

    let flying = serde_json::json!({
        "armed": true,
        "mode": "GUIDED",
        "position": {"lat": 12.34, "lon": 56.78, "alt_rel": 40.0},
        "velocity": {"vx": 8.0, "vy": 0.0, "vz": 0.0},
        "gps": {"fix_type": 3},
        "video_profile": "hero",
        "swarm_emergency": true,
        "swarm_precedence": "hard-separation",
    });

    // The producer is alive: this snapshot arrived just now. The ONLY
    // difference between the two cases below is how old the FC's position
    // fix is.
    let with_fix_age = |ms: u64| StateSnapshot {
        value: {
            let mut v = flying.clone();
            v["position_age_ms"] = serde_json::json!(ms);
            v
        },
        received: Instant::now(),
    };
    let now = Instant::now();

    let good = own_beacon(Some(&with_fix_age(50)), now, 4, 0);
    assert_ne!(good.lat, 0, "a live fix is broadcast");
    assert_ne!(good.vx_cms, 0);
    assert!(good.gps_ok());
    assert!(good.armed() && good.guided() && good.hero() && good.emergency());

    let frozen = with_fix_age(OWN_STATE_STALE.as_millis() as u64 + 500);
    let suppressed = own_beacon(Some(&frozen), now, 4, 0);
    assert_eq!(suppressed.lat, 0, "a frozen fix must not be radiated");
    assert_eq!(suppressed.lon, 0);
    assert_eq!(suppressed.alt_dm, 0);
    assert_eq!(
        suppressed.vx_cms, 0,
        "a velocity the neighbours dead-reckon forward is the actual hazard"
    );
    assert!(
        !suppressed.gps_ok(),
        "GPS_OK on a dead fix is the bit that makes a neighbour trust it"
    );
    assert!(suppressed.armed(), "still armed");
    assert!(suppressed.guided(), "still in its mode");
    assert!(suppressed.hero(), "still the hero");
    assert!(suppressed.emergency(), "still in override");
    assert_eq!(suppressed.precedence(), ModePrecedence::HardSeparation);
    assert_eq!(suppressed.slot, 4, "the node is still on the bus");
}

/// A producer that does not state a fix age has not earned the fleet's
/// trust in its position. Absence must fail CLOSED for the position: reading
/// it as "no reason to worry" is exactly how a frozen fix gets radiated.
#[test]
fn a_snapshot_with_no_stated_fix_age_radiates_no_position() {
    let no_age = StateSnapshot {
        value: serde_json::json!({
            "armed": true,
            "position": {"lat": 12.34, "lon": 56.78},
            "gps": {"fix_type": 3},
        }),
        received: Instant::now(),
    };
    let b = own_beacon(Some(&no_age), Instant::now(), 2, 0);
    assert_eq!((b.lat, b.lon), (0, 0));
    assert!(!b.gps_ok());
    assert!(b.armed());

    // An explicit null (the producer has never decoded a POSITION) reads the
    // same way, and must never read as age zero.
    let never_fixed = StateSnapshot {
        value: serde_json::json!({"armed": true, "position_age_ms": null}),
        received: Instant::now(),
    };
    assert_eq!(
        beacon_body(Some(&never_fixed), Instant::now()).map(|(_, t)| t),
        Some(false)
    );
}

/// Both boundaries are exclusive on the near side, matching the control
/// loop's own reading of the same window.
#[test]
fn the_freshness_window_is_the_shared_one() {
    let held = StateSnapshot {
        value: serde_json::json!({"armed": true, "position_age_ms": 0}),
        received: Instant::now(),
    };
    let just_inside = held.received + OWN_STATE_STALE - Duration::from_millis(1);
    assert_eq!(
        beacon_body(Some(&held), just_inside).map(|(_, t)| t),
        Some(true)
    );
    assert!(beacon_body(Some(&held), held.received + OWN_STATE_STALE).is_none());
    assert!(beacon_body(None, Instant::now()).is_none());

    let at_edge = |ms: u64| StateSnapshot {
        value: serde_json::json!({"position_age_ms": ms}),
        received: held.received,
    };
    let edge = OWN_STATE_STALE.as_millis() as u64;
    let trusted = |s: &StateSnapshot| beacon_body(Some(s), held.received).map(|(_, t)| t);
    assert_eq!(trusted(&at_edge(edge - 1)), Some(true));
    assert_eq!(trusted(&at_edge(edge)), Some(false));
}

/// The state reader used to break out of its read loop leaving the last
/// snapshot in the cell, so a flight controller that stopped publishing left
/// its final reading standing while the reader reconnected.
#[tokio::test]
async fn a_closed_state_stream_clears_the_held_snapshot() {
    use ados_protocol::ipc::IpcBroadcast;
    use ados_protocol::state::encode_v2;

    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("state.sock");
    let (server, _in) = IpcBroadcast::bind(&sock, 32, true, None).await.unwrap();
    let (cancel, cancel_rx) = watch::channel(false);
    let (shared, _reader) = spawn_state_reader(sock.to_string_lossy().into_owned(), cancel_rx);

    server
        .broadcast(
            encode_v2(&serde_json::json!({"armed": true}))
                .unwrap()
                .into(),
        )
        .await;
    for _ in 0..100 {
        if shared.lock().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(shared.lock().is_some(), "the snapshot must be held");

    // The producer goes away.
    drop(server);
    for _ in 0..100 {
        if shared.lock().is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        shared.lock().is_none(),
        "a snapshot with no producer behind it must not stay held"
    );
    let _ = cancel.send(true);
}

/// A stop requested before the service reached its own await point must still
/// stop it. With a non-latched notification, a stop that lands during startup
/// was lost and the service ran on until systemd killed it, leaving
/// `swarm.sock` behind.
#[tokio::test]
async fn a_stop_requested_during_startup_still_stops_the_service() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = SwarmBusConfig {
        ground_station: true,
        interface: "nonexistent-swarm-iface0".to_string(),
        fleet_slot: ados_radio::config::SLOT_GROUND,
        socket_dir: dir.path().to_string_lossy().into_owned(),
        replay_state_path: dir
            .path()
            .join("replay.json")
            .to_string_lossy()
            .into_owned(),
        ..SwarmBusConfig::default()
    };
    let (cancel, cancel_rx) = watch::channel(false);
    // The stop lands before `run` has registered anything.
    cancel.send(true).unwrap();
    let finished = tokio::time::timeout(Duration::from_secs(5), run(cfg.clone(), cancel_rx)).await;
    assert!(finished.is_ok(), "the service must honour an early stop");
    assert!(
        !Path::new(&cfg.swarm_socket_path()).exists(),
        "shutdown removes the socket"
    );
}

/// A frame heard before a restart is still a replay after it. Without the
/// persisted window the restarted receiver read the slot as never heard,
/// accepted the captured frame as fresh, and then refused the real sender's
/// next run as a second sender while the replay kept the slot live.
#[test]
fn a_frame_heard_before_a_restart_is_refused_after_it() {
    use crate::beacon::SwarmBeacon;
    use crate::crypto::SenderNonce;
    use crate::neighbors::Recorded;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("swarmbus-replay.json");
    let t0 = Instant::now();
    let beacon = SwarmBeacon {
        slot: 3,
        ..SwarmBeacon::default()
    };
    let run = |counter| SenderNonce {
        prefix: [0x5A; 8],
        counter,
    };

    let before = Mutex::new(NeighborTable::new(1));
    assert_eq!(
        before.lock().record(beacon, run(40), None, t0),
        Recorded::Accepted
    );
    persist_replay(&path, &before);
    drop(before);

    // The restarted receiver: empty neighbour table, window read back.
    let mut after = NeighborTable::with_sender_marks(1, SenderMarks::load(&path));
    assert!(after.is_empty());
    assert_eq!(after.record(beacon, run(40), None, t0), Recorded::Replayed);
    assert_eq!(after.record(beacon, run(12), None, t0), Recorded::Replayed);
    // The same sender carrying on is still heard.
    assert_eq!(after.record(beacon, run(41), None, t0), Recorded::Accepted);
}

/// Writes happen only when something was accepted, and a failed write is
/// retried on the next call rather than lost.
#[test]
fn the_window_is_written_only_when_it_changed_and_a_failed_write_retries() {
    use crate::beacon::SwarmBeacon;
    use crate::crypto::SenderNonce;

    let dir = tempfile::tempdir().unwrap();
    let table = Mutex::new(NeighborTable::new(1));
    let path = dir.path().join("replay.json");
    persist_replay(&path, &table);
    assert!(!path.exists(), "nothing heard, nothing written");

    let beacon = SwarmBeacon {
        slot: 3,
        ..SwarmBeacon::default()
    };
    let sender = SenderNonce {
        prefix: [1; 8],
        counter: 0,
    };
    table.lock().record(beacon, sender, None, Instant::now());
    // A regular file where the parent directory should be: the atomic
    // writer creates missing parents, so only a non-directory parent fails.
    std::fs::write(dir.path().join("blocker"), b"").unwrap();
    let unwritable = dir.path().join("blocker/replay.json");
    persist_replay(&unwritable, &table);
    assert!(!unwritable.exists());
    persist_replay(&path, &table);
    assert!(path.exists(), "the failed write was retried");
}

/// A row accepted under the old key is gone after a re-key, nothing is
/// published until a beacon under the new key arrives, and the replay marks
/// survive so the old frame cannot be replayed back in.
#[test]
fn a_rekey_drops_rows_from_the_old_key_but_keeps_replay_marks() {
    use crate::beacon::SwarmBeacon;
    use crate::crypto::SenderNonce;
    use crate::neighbors::Recorded;

    let t0 = Instant::now();
    let beacon = SwarmBeacon {
        slot: 3,
        ..SwarmBeacon::default()
    };
    let run = |counter| SenderNonce {
        prefix: [0x5A; 8],
        counter,
    };
    let mut table = NeighborTable::new(1);
    assert_eq!(table.record(beacon, run(40), None, t0), Recorded::Accepted);
    assert_eq!(table.len(), 1);

    assert_eq!(forget_neighbors_on_rekey(&mut table, t0), 1);
    assert!(table.is_empty(), "no neighbour survives the re-key");
    assert_eq!(
        table.record(beacon, run(40), None, t0),
        Recorded::Replayed,
        "the replay marks are kept"
    );
    assert!(table.is_empty());
    // A fresh beacon after the re-key repopulates the table.
    assert_eq!(table.record(beacon, run(41), None, t0), Recorded::Accepted);
    assert_eq!(table.len(), 1);
}
