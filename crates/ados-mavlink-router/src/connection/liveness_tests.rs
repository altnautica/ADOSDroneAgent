use super::*;
use crate::param_cache::ParamCache;
use crate::state::VehicleState;

fn conn_with(cfg: MavlinkConfig) -> std::sync::Arc<FcConnection> {
    let state = std::sync::Arc::new(Mutex::new(VehicleState::default()));
    let params = std::sync::Arc::new(Mutex::new(ParamCache::new(
        "/tmp/ados-liveness-params.json",
    )));
    FcConnection::new(cfg, state, params)
}

#[tokio::test]
async fn fresh_connection_is_transport_closed_and_not_alive() {
    let c = conn_with(MavlinkConfig::default());
    assert!(!c.transport_open());
    assert!(!c.mavlink_alive().await);
    assert!(!c.connected().await);
    assert!(c.heartbeat_age_s().await.is_none());
}

#[tokio::test]
async fn transport_open_without_a_heartbeat_is_not_connected() {
    // The exact bug: the port opens but no HEARTBEAT decodes → the link is
    // transport-open but NOT alive, so fc_connected must be false.
    let c = conn_with(MavlinkConfig::default());
    c.connected.store(true, Ordering::Relaxed);
    assert!(c.transport_open());
    assert!(!c.mavlink_alive().await, "no heartbeat → not alive");
    assert!(
        !c.connected().await,
        "transport open alone is not connected"
    );
}

#[tokio::test]
async fn a_fresh_heartbeat_makes_an_open_transport_connected() {
    let c = conn_with(MavlinkConfig::default());
    c.connected.store(true, Ordering::Relaxed);
    *c.last_heartbeat_at.lock().await = Some(Instant::now());
    assert!(c.mavlink_alive().await);
    assert!(c.connected().await);
    let age = c
        .heartbeat_age_s()
        .await
        .expect("age present after a heartbeat");
    assert!((0.0..1.0).contains(&age));
}

#[tokio::test]
async fn a_stale_heartbeat_reads_not_alive() {
    let c = conn_with(MavlinkConfig::default());
    c.connected.store(true, Ordering::Relaxed);
    *c.last_heartbeat_at.lock().await =
        Some(Instant::now() - (HEARTBEAT_TIMEOUT + Duration::from_secs(1)));
    assert!(!c.mavlink_alive().await, "stale heartbeat → not alive");
    assert!(!c.connected().await);
}

#[tokio::test]
async fn link_hint_is_none_when_alive_or_no_transport() {
    let c = conn_with(MavlinkConfig::default());
    // Fresh connection: transport closed → nothing to explain.
    assert_eq!(c.link_hint().await, "none");
    // Transport open with a fresh heartbeat → alive → still none.
    c.connected.store(true, Ordering::Relaxed);
    *c.last_heartbeat_at.lock().await = Some(Instant::now());
    assert_eq!(c.link_hint().await, "none");
}

#[tokio::test]
async fn link_hint_is_no_heartbeat_when_open_and_silent() {
    let c = conn_with(MavlinkConfig::default());
    c.connected.store(true, Ordering::Relaxed);
    // Transport open, no heartbeat, no MSP evidence → no_heartbeat.
    assert_eq!(c.link_hint().await, "no_heartbeat");
}

#[tokio::test]
async fn link_hint_is_msp_detected_with_recent_evidence_and_clears_on_heartbeat() {
    let c = conn_with(MavlinkConfig::default());
    c.connected.store(true, Ordering::Relaxed);
    *c.last_msp_at.lock().await = Some(Instant::now());
    assert_eq!(c.link_hint().await, "msp_detected");
    // A fresh HEARTBEAT (alive) overrides the MSP hint immediately.
    *c.last_heartbeat_at.lock().await = Some(Instant::now());
    assert_eq!(c.link_hint().await, "none");
}

#[tokio::test]
async fn link_hint_msp_evidence_ages_out() {
    let c = conn_with(MavlinkConfig::default());
    c.connected.store(true, Ordering::Relaxed);
    *c.last_msp_at.lock().await = Some(Instant::now() - (MSP_HINT_TTL + Duration::from_secs(1)));
    // Stale MSP evidence no longer arms the hint; falls back to no_heartbeat.
    assert_eq!(c.link_hint().await, "no_heartbeat");
}

#[tokio::test]
async fn link_hint_is_source_unreachable_for_a_failed_configured_source() {
    // A configured tcp source whose endpoint never opens (the run loop set
    // open_failed) is unreachable, not merely a silent FC.
    let c = conn_with(MavlinkConfig {
        source: "tcp".into(),
        serial_port: "tcp:203.0.113.9:15760".into(),
        ..MavlinkConfig::default()
    });
    c.open_failed.store(true, Ordering::Relaxed);
    assert!(!c.transport_open());
    assert_eq!(c.link_hint().await, "source_unreachable");
}

#[tokio::test]
async fn link_hint_auto_discovery_stays_none_even_when_open_fails() {
    // Auto-discovery has no configured endpoint to be "unreachable", so a
    // failed probe stays quiet (nothing for the operator to fix).
    let c = conn_with(MavlinkConfig::default());
    c.open_failed.store(true, Ordering::Relaxed);
    assert_eq!(c.source(), "auto");
    assert_eq!(c.link_hint().await, "none");
}

#[tokio::test]
async fn link_hint_clears_source_unreachable_once_a_transport_opens() {
    let c = conn_with(MavlinkConfig {
        source: "tcp".into(),
        serial_port: "tcp:203.0.113.9:15760".into(),
        ..MavlinkConfig::default()
    });
    c.open_failed.store(true, Ordering::Relaxed);
    assert_eq!(c.link_hint().await, "source_unreachable");
    // The transport opened (run loop clears open_failed); the link is now
    // open-and-silent, so the hint moves to no_heartbeat.
    c.open_failed.store(false, Ordering::Relaxed);
    c.connected.store(true, Ordering::Relaxed);
    assert_eq!(c.link_hint().await, "no_heartbeat");
}

/// Serialise one HEARTBEAT as the given component.
fn heartbeat_from(
    system_id: u8,
    component_id: u8,
    mavtype: ados_protocol::mavlink::ardupilotmega::MavType,
    autopilot: ados_protocol::mavlink::ardupilotmega::MavAutopilot,
    armed: bool,
) -> Vec<u8> {
    use ados_protocol::mavlink::ardupilotmega::{MavModeFlag, MavState, HEARTBEAT_DATA};
    let msg = MavMessage::HEARTBEAT(HEARTBEAT_DATA {
        custom_mode: 4,
        mavtype,
        autopilot,
        base_mode: if armed {
            MavModeFlag::MAV_MODE_FLAG_SAFETY_ARMED
        } else {
            MavModeFlag::empty()
        },
        system_status: MavState::MAV_STATE_ACTIVE,
        mavlink_version: 3,
    });
    let header = MavHeader {
        system_id,
        component_id,
        sequence: 0,
    };
    mavlink::serialize_v2(header, &msg).unwrap()
}

/// A flight controller forwards a ground station's HEARTBEAT (from a
/// telemetry radio on another port) and a companion's to this link. Only the
/// autopilot's describes the vehicle: the ground station's must not disarm
/// it in the snapshot or re-point requests at the ground station.
#[tokio::test]
async fn a_forwarded_ground_station_heartbeat_never_drives_the_vehicle() {
    use ados_protocol::mavlink::ardupilotmega::{MavAutopilot, MavType};
    let c = conn_with(MavlinkConfig::default());
    let mut wire = heartbeat_from(
        1,
        1,
        MavType::MAV_TYPE_QUADROTOR,
        MavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA,
        true,
    );
    wire.extend(heartbeat_from(
        255,
        190,
        MavType::MAV_TYPE_GCS,
        MavAutopilot::MAV_AUTOPILOT_INVALID,
        false,
    ));
    wire.extend(heartbeat_from(
        1,
        191,
        MavType::MAV_TYPE_ONBOARD_CONTROLLER,
        MavAutopilot::MAV_AUTOPILOT_INVALID,
        false,
    ));
    c.read_loop(Box::pin(std::io::Cursor::new(wire))).await;

    let s = c.state.lock().await;
    assert!(
        s.armed,
        "the ground station's base mode must not disarm the vehicle"
    );
    assert_eq!(s.mode, "GUIDED");
    assert_eq!(s.mav_type, MavType::MAV_TYPE_QUADROTOR as i64);
    drop(s);
    assert_eq!(c.target_system.load(Ordering::Relaxed), 1);
    assert!(c.last_heartbeat_at.lock().await.is_some());
}

/// A link that only carries somebody else's HEARTBEAT is not a live
/// vehicle link.
#[tokio::test]
async fn a_ground_station_heartbeat_alone_does_not_make_the_link_alive() {
    use ados_protocol::mavlink::ardupilotmega::{MavAutopilot, MavType};
    let c = conn_with(MavlinkConfig::default());
    c.connected.store(true, Ordering::Relaxed);
    let wire = heartbeat_from(
        255,
        190,
        MavType::MAV_TYPE_GCS,
        MavAutopilot::MAV_AUTOPILOT_INVALID,
        false,
    );
    c.read_loop(Box::pin(std::io::Cursor::new(wire))).await;
    assert!(!c.mavlink_alive().await);
    assert_eq!(c.target_system.load(Ordering::Relaxed), 1);
}

#[test]
fn source_infers_from_serial_port_shape_when_unset() {
    // An empty `source` (a config that predates the field) is inferred from
    // the connection-string shape so the picker still reflects reality.
    // Empty port → auto-detect.
    assert_eq!(
        conn_source(MavlinkConfig {
            source: String::new(),
            serial_port: String::new(),
            ..MavlinkConfig::default()
        }),
        "auto"
    );
    // A udp:/tcp: prefix is a network transport.
    assert_eq!(
        conn_source(MavlinkConfig {
            source: String::new(),
            serial_port: "udp:127.0.0.1:14550".into(),
            ..MavlinkConfig::default()
        }),
        "udp"
    );
    assert_eq!(
        conn_source(MavlinkConfig {
            source: String::new(),
            serial_port: "tcp:127.0.0.1:5760".into(),
            ..MavlinkConfig::default()
        }),
        "tcp"
    );
    // A bare device path is serial.
    assert_eq!(
        conn_source(MavlinkConfig {
            source: String::new(),
            serial_port: "/dev/ttyACM0".into(),
            ..MavlinkConfig::default()
        }),
        "serial"
    );
}

#[test]
fn source_honors_an_explicit_value() {
    // An explicit pick is reported verbatim (NOT re-inferred): "auto" means
    // the operator chose agent-decides, even with a serial_port present.
    assert_eq!(
        conn_source(MavlinkConfig {
            source: "auto".into(),
            serial_port: "/dev/ttyACM0".into(),
            ..MavlinkConfig::default()
        }),
        "auto"
    );
    for kind in ["serial", "udp", "tcp", "auto"] {
        assert_eq!(
            conn_source(MavlinkConfig {
                source: kind.into(),
                ..MavlinkConfig::default()
            }),
            kind
        );
    }
}

fn conn_source(cfg: MavlinkConfig) -> &'static str {
    conn_with(cfg).source()
}

#[tokio::test(start_paused = true)]
async fn the_fc_reconnect_retries_on_a_flat_interval_with_no_attempt_cap() {
    // This is the sole C2 path with
    // no packaged fallback, so the retry must be flat: the old ladder walked
    // 1→2→4→8→16→30 s (and never reset for a port that opens but never
    // accepts a write), leaving a drone up to half a minute with no FC link
    // after a replug or a brownout.
    //
    // A real listener that accepts and immediately drops each connection
    // turns every reconnect attempt into one accept, stamped with the paused
    // clock. The test awaits each accept in turn instead of counting after a
    // fixed number of yields, so the result does not depend on how the
    // scheduler interleaves the loopback I/O with the clock.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener bind");
    let addr = listener.local_addr().expect("listener addr");
    let (accepted_tx, mut accepted) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        // Drop each peer immediately: the router sees EOF, tears the
        // session down and re-opens after the fixed interval.
        while let Ok((peer, _)) = listener.accept().await {
            if accepted_tx.send(tokio::time::Instant::now()).is_err() {
                return;
            }
            drop(peer);
        }
    });

    let c = conn_with(MavlinkConfig {
        source: "tcp".into(),
        serial_port: format!("tcp:{}:{}", addr.ip(), addr.port()),
        ..MavlinkConfig::default()
    });
    let cancel = Shutdown::new();
    let run_conn = c.clone();
    let run_cancel = cancel.clone();
    let mut run_task = tokio::spawn(async move { run_conn.run(run_cancel).await });

    // The only timer in play is the reconnect sleep, so the paused clock
    // auto-advances by exactly one interval between consecutive accepts. A
    // doubling ladder would stretch the later gaps, and an attempt cap would
    // end the run task before the attempts below were all seen.
    const ATTEMPTS: usize = 12;
    let mut stamps = Vec::with_capacity(ATTEMPTS);
    while stamps.len() < ATTEMPTS {
        tokio::select! {
            at = accepted.recv() => stamps.push(at.expect("the listener stays up")),
            _ = &mut run_task => panic!(
                "the reconnect loop stopped after {} attempts",
                stamps.len()
            ),
        }
    }
    cancel.trigger();

    for (i, pair) in stamps.windows(2).enumerate() {
        let gap = pair[1] - pair[0];
        assert!(
            gap >= RECONNECT_INTERVAL && gap < RECONNECT_INTERVAL * 2,
            "retry {} came {gap:?} after the previous one; the interval must stay flat",
            i + 1
        );
    }
    // And the interval itself stays inside the fixed recovery band.
    assert!(RECONNECT_INTERVAL >= Duration::from_secs(2));
    assert!(RECONNECT_INTERVAL <= Duration::from_secs(5));
}

fn cand(name: &str, usb: Option<(u16, u16)>) -> FcCandidate {
    FcCandidate {
        name: name.into(),
        usb,
    }
}

#[test]
fn serial_candidates_probe_flight_controllers_first_and_onboard_uarts_last() {
    // Enumeration order on a companion computer often lists the onboard
    // UARTs first. Probed in that order, an idle UART ahead of the FC used
    // to be the port the router latched onto.
    let mut ports = [
        cand("/dev/ttyAMA0", None),
        cand("/dev/ttyS0", None),
        cand("/dev/ttyUSB0", Some((0x10C4, 0xEA60))),
        cand("/dev/ttyACM0", Some((0x1234, 0x0001))),
        cand("/dev/ttyACM1", Some((0x1209, 0x5741))),
    ];
    ports.sort_by_key(candidate_rank);
    let order: Vec<&str> = ports.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        order,
        [
            "/dev/ttyACM1",
            "/dev/ttyACM0",
            "/dev/ttyUSB0",
            "/dev/ttyAMA0",
            "/dev/ttyS0"
        ]
    );
}

#[test]
fn a_silent_onboard_uart_is_never_opened_on_a_guess_beside_other_candidates() {
    let uart = cand("/dev/ttyAMA0", None);
    let usb = cand("/dev/ttyACM0", Some((0x1209, 0x5741)));
    assert!(!fallback_open_allowed(false, &uart));
    assert!(fallback_open_allowed(false, &usb));
    // The sole candidate (or the operator's configured port) is still tried.
    assert!(fallback_open_allowed(true, &uart));
}

#[tokio::test(start_paused = true)]
async fn an_unproven_link_that_stays_silent_is_released_for_a_new_sweep() {
    let c = conn_with(MavlinkConfig::default());
    c.link_proven.store(false, Ordering::Relaxed);
    let started = tokio::time::Instant::now();
    tokio::time::timeout(UNPROVEN_LINK_TIMEOUT * 2, c.unproven_link_expired())
        .await
        .expect("a silent unproven link is released");
    assert!(started.elapsed() >= UNPROVEN_LINK_TIMEOUT);
}

#[tokio::test(start_paused = true)]
async fn an_unproven_link_that_shows_a_heartbeat_is_kept() {
    let c = conn_with(MavlinkConfig::default());
    c.link_proven.store(false, Ordering::Relaxed);
    let watch = c.clone();
    let expiry = tokio::spawn(async move { watch.unproven_link_expired().await });
    tokio::time::sleep(UNPROVEN_LINK_TIMEOUT / 2).await;
    // The read loop marks the session proven on the vehicle's HEARTBEAT.
    c.link_proven.store(true, Ordering::Relaxed);
    tokio::time::sleep(UNPROVEN_LINK_TIMEOUT * 3).await;
    assert!(!expiry.is_finished(), "a proven link is never released");
    expiry.abort();
}
