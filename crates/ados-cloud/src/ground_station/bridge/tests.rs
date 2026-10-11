use super::*;

fn transport() -> TransportConfig {
    TransportConfig {
        client_id: "ados-dev1".to_string(),
        host: "mqtt.example".to_string(),
        port: 443,
        wire: crate::mqtt::BrokerWire::Wss,
        ws_path: "/mqtt".to_string(),
        username: "ados-dev1".to_string(),
        password: "k".to_string(),
        inflight: 1000,
        keep_alive: Duration::from_secs(30),
    }
}

fn bridge() -> CloudRelayBridge {
    CloudRelayBridge::new("dev1", Some("paired-drone".to_string()), transport())
}

struct FixedState(serde_json::Value);
impl StateSnapshotSource for FixedState {
    fn latest(&self) -> Option<serde_json::Value> {
        Some(self.0.clone())
    }
}

#[test]
fn throttle_string_round_trips_and_full_forwarding_truth_table() {
    // `as_str()` is the wire string carried on the GS heartbeat; it must be
    // the exact inverse of `from_cap_str()` for every level so a level that
    // rides in on the active-flag file round-trips back out on the heartbeat.
    for (cap, level) in [
        ("ok", ThrottleState::None),
        ("warn_80", ThrottleState::Warn),
        ("throttle_95", ThrottleState::VideoOff),
        ("blocked_100", ThrottleState::Blocked),
    ] {
        assert_eq!(ThrottleState::from_cap_str(cap), level);
    }
    // `as_str()` emits the canonical cap string for each non-default level,
    // and `from_cap_str(as_str())` is the identity (the default level emits
    // "none", which parses back to None).
    assert_eq!(ThrottleState::None.as_str(), "none");
    assert_eq!(ThrottleState::Warn.as_str(), "warn_80");
    assert_eq!(ThrottleState::VideoOff.as_str(), "throttle_95");
    assert_eq!(ThrottleState::Blocked.as_str(), "blocked_100");
    for level in [
        ThrottleState::None,
        ThrottleState::Warn,
        ThrottleState::VideoOff,
        ThrottleState::Blocked,
    ] {
        assert_eq!(
            ThrottleState::from_cap_str(level.as_str()),
            level,
            "as_str() round-trips through from_cap_str()"
        );
    }

    // The full forward truth table across every level.
    assert!(ThrottleState::None.forward_video());
    assert!(ThrottleState::None.forward_telemetry());
    assert!(ThrottleState::Warn.forward_video());
    assert!(ThrottleState::Warn.forward_telemetry());
    assert!(!ThrottleState::VideoOff.forward_video());
    assert!(ThrottleState::VideoOff.forward_telemetry());
    assert!(!ThrottleState::Blocked.forward_video());
    assert!(!ThrottleState::Blocked.forward_telemetry());
}

#[test]
fn throttle_maps_cap_strings_and_forwarding_rules() {
    assert_eq!(ThrottleState::from_cap_str("ok"), ThrottleState::None);
    assert_eq!(ThrottleState::from_cap_str("warn_80"), ThrottleState::Warn);
    assert_eq!(
        ThrottleState::from_cap_str("throttle_95"),
        ThrottleState::VideoOff
    );
    assert_eq!(
        ThrottleState::from_cap_str("blocked_100"),
        ThrottleState::Blocked
    );
    // 95 %: video off, telemetry on.
    assert!(!ThrottleState::VideoOff.forward_video());
    assert!(ThrottleState::VideoOff.forward_telemetry());
    // 100 %: both off.
    assert!(!ThrottleState::Blocked.forward_video());
    assert!(!ThrottleState::Blocked.forward_telemetry());
    // ok / warn: both on.
    assert!(ThrottleState::None.forward_video());
    assert!(ThrottleState::Warn.forward_video());
}

#[test]
fn the_relay_retry_interval_is_flat_and_inside_the_recovery_band() {
    // The ladder this replaces doubled 2 s → 300 s, which left a node
    // invisible to the cloud fleet view for up to five minutes after the
    // uplink recovered, with no operator-reachable way to force a retry.
    assert!(RELAY_RETRY_INTERVAL >= Duration::from_secs(2));
    assert!(RELAY_RETRY_INTERVAL <= Duration::from_secs(5));
}

#[test]
fn first_uplink_with_reachability_triggers_teardown_then_reconnect() {
    let mut br = bridge();
    let snap = UplinkSnapshot {
        active_uplink: "eth0".to_string(),
        internet_reachable: true,
        data_cap_state: "ok".to_string(),
    };
    assert_eq!(
        br.reconcile_uplink(Some(&snap)),
        MqttAction::TeardownThenReconnect
    );
    // Same snapshot again → no change.
    assert_eq!(br.reconcile_uplink(Some(&snap)), MqttAction::Noop);
}

#[test]
fn uplink_change_always_tears_down_then_reconnects_over_new_iface() {
    let mut br = bridge();
    let eth = UplinkSnapshot {
        active_uplink: "eth0".to_string(),
        internet_reachable: true,
        data_cap_state: "ok".to_string(),
    };
    br.reconcile_uplink(Some(&eth));
    // Failover to cellular: a different uplink → explicit teardown+reconnect
    // (the whole point — rumqttc would NOT re-bind to wwan0 on its own).
    let wwan = UplinkSnapshot {
        active_uplink: "wwan0".to_string(),
        internet_reachable: true,
        data_cap_state: "ok".to_string(),
    };
    assert_eq!(
        br.reconcile_uplink(Some(&wwan)),
        MqttAction::TeardownThenReconnect
    );
}

#[test]
fn losing_all_uplinks_tears_down_to_idle() {
    let mut br = bridge();
    let eth = UplinkSnapshot {
        active_uplink: "eth0".to_string(),
        internet_reachable: true,
        data_cap_state: "ok".to_string(),
    };
    br.reconcile_uplink(Some(&eth));
    // File gone → no uplink → idle.
    assert_eq!(br.reconcile_uplink(None), MqttAction::TeardownIdle);
}

#[test]
fn health_flip_on_same_uplink_brings_up_or_tears_down() {
    let mut br = bridge();
    // Start with a reachable uplink and mark MQTT connected.
    let up = UplinkSnapshot {
        active_uplink: "eth0".to_string(),
        internet_reachable: true,
        data_cap_state: "ok".to_string(),
    };
    br.reconcile_uplink(Some(&up));
    br.mqtt_connected = true;
    // Reachability lost on the same uplink → teardown.
    let down = UplinkSnapshot {
        active_uplink: "eth0".to_string(),
        internet_reachable: false,
        data_cap_state: "ok".to_string(),
    };
    assert_eq!(br.reconcile_uplink(Some(&down)), MqttAction::TeardownIdle);
    br.mqtt_connected = false;
    // Reachability restored → bring up.
    assert_eq!(br.reconcile_uplink(Some(&up)), MqttAction::BringUp);
}

#[test]
fn data_cap_downshift_blocks_and_recovers() {
    let mut br = bridge();
    // 95 %: video off, telemetry kept, relay stays up.
    assert!(!br.apply_data_cap(ThrottleState::VideoOff));
    assert!(!br.forwarding_video());
    assert!(br.forwarding_telemetry());
    // 100 %: heartbeat-only, relay torn down.
    assert!(br.apply_data_cap(ThrottleState::Blocked));
    assert!(!br.forwarding_video());
    assert!(!br.forwarding_telemetry());
    // Recover to ok.
    assert!(!br.apply_data_cap(ThrottleState::None));
    assert!(br.forwarding_video());
    assert!(br.forwarding_telemetry());
}

#[test]
fn cap_only_change_on_a_stable_uplink_is_noop_but_updates_the_throttle() {
    let mut br = bridge();
    // Establish a stable, reachable uplink at "ok".
    let ok = UplinkSnapshot {
        active_uplink: "eth0".to_string(),
        internet_reachable: true,
        data_cap_state: "ok".to_string(),
    };
    br.reconcile_uplink(Some(&ok));
    assert_eq!(br.throttle_state(), ThrottleState::None);
    // The SAME uplink + reachability, only the cap level rises: no MQTT
    // action (the uplink did not change), but the throttle is folded in so
    // the next heartbeat + the forwarding decisions track the cap.
    let warn = UplinkSnapshot {
        active_uplink: "eth0".to_string(),
        internet_reachable: true,
        data_cap_state: "warn_80".to_string(),
    };
    assert_eq!(br.reconcile_uplink(Some(&warn)), MqttAction::Noop);
    assert_eq!(br.throttle_state(), ThrottleState::Warn);
    // Rise further to the video-off threshold on the same stable uplink.
    let throttle95 = UplinkSnapshot {
        active_uplink: "eth0".to_string(),
        internet_reachable: true,
        data_cap_state: "throttle_95".to_string(),
    };
    assert_eq!(br.reconcile_uplink(Some(&throttle95)), MqttAction::Noop);
    assert_eq!(br.throttle_state(), ThrottleState::VideoOff);
    assert!(!br.forwarding_video());
    assert!(br.forwarding_telemetry());
}

#[test]
fn data_cap_rides_in_on_the_uplink_snapshot() {
    let mut br = bridge();
    let snap = UplinkSnapshot {
        active_uplink: "wwan0".to_string(),
        internet_reachable: true,
        data_cap_state: "throttle_95".to_string(),
    };
    br.reconcile_uplink(Some(&snap));
    // The cap level carried on the same file is applied.
    assert_eq!(br.throttle_state(), ThrottleState::VideoOff);
    assert!(!br.forwarding_video());
    assert!(br.forwarding_telemetry());
}

#[test]
fn heartbeat_is_none_without_an_uplink_and_carries_relay_state_with_one() {
    let mut br = bridge();
    // No uplink yet → no heartbeat.
    assert!(br.build_heartbeat(1).is_none());
    // After an uplink, the heartbeat carries the relay's forwarding state.
    let snap = UplinkSnapshot {
        active_uplink: "eth0".to_string(),
        internet_reachable: true,
        data_cap_state: "throttle_95".to_string(),
    };
    br.reconcile_uplink(Some(&snap));
    let hb = br.build_heartbeat(1234).unwrap();
    assert_eq!(hb.uplink, "eth0");
    // The heartbeat carries the ground station's OWN device id, serialized
    // as `deviceId` to match the /agent/status auth contract (not the cloud
    // owner id the prior cut wrongly sent).
    assert_eq!(hb.device_id, "dev1");
    assert_eq!(hb.throttle_state, "throttle_95");
    assert!(!hb.forwarding_video);
    assert!(hb.forwarding_telemetry);
    assert_eq!(hb.ts_ms, 1234);
    // No state source → no telemetry block.
    assert!(hb.telemetry.is_none());
    // The wire payload keys the device on `deviceId` (the /agent/status auth
    // contract the handler reads), never the old unread `drone_id`, and the
    // whole document is camelCase so it lands in the status mutation in its
    // canonical shape (the handler does not remap top-level fields).
    let wire = serde_json::to_value(&hb).unwrap();
    assert_eq!(wire["deviceId"], "dev1");
    assert!(wire.get("drone_id").is_none());
    assert_eq!(wire["throttleState"], "throttle_95");
    assert_eq!(wire["forwardingVideo"], false);
    assert_eq!(wire["forwardingTelemetry"], true);
    assert!(wire.get("mqtt_connected").is_none());
    // The status-mutation contract a drone meets: version + uptimeSeconds +
    // a discriminating profile, so the GS registers as a ground-station node.
    assert_eq!(wire["profile"], "ground-station");
    assert!(wire.get("version").and_then(|v| v.as_str()).is_some());
    assert!(wire.get("uptimeSeconds").and_then(|v| v.as_i64()).is_some());
    // No role to report yet → the field is omitted, not null.
    assert!(wire.get("role").is_none());
}

#[test]
fn the_relay_state_is_published_for_the_single_status_producer() {
    // The bridge must not be a second /agent/status producer: it hands its
    // relay block to the heartbeat loop, which folds it onto the one POST.
    let (tx, rx) = watch::channel(None);
    let mut br = bridge().with_relay_state_sink(tx);
    let snap = UplinkSnapshot {
        active_uplink: "wlan0".to_string(),
        internet_reachable: true,
        data_cap_state: "ok".to_string(),
    };
    br.reconcile_uplink(Some(&snap));
    br.publish_relay_state(br.build_heartbeat(99));
    let published = rx.borrow().clone().expect("relay state is published");
    assert_eq!(published.uplink, "wlan0");
    assert_eq!(published.profile, "ground-station");
    assert_eq!(published.ts_ms, 99);
    // Stopping withdraws the block, so a dead bridge's last forwarding
    // state is not folded onto every later heartbeat.
    br.publish_relay_state(None);
    assert!(rx.borrow().is_none());
}

#[test]
fn heartbeat_folds_live_telemetry_when_a_state_source_is_wired() {
    // The router's real nested wire shape.
    let state = serde_json::json!({
        "armed": true, "mode": "GUIDED",
        "position": {"lat": 12.97, "lon": 77.59, "alt_msl": 930.0, "alt_rel": 30.0},
        "battery": {"voltage": 16.2, "current": 3.1, "remaining": 88},
        "last_heartbeat": 1700000000.0
    });
    let mut br = bridge().with_state_source(std::sync::Arc::new(FixedState(state)));
    let snap = UplinkSnapshot {
        active_uplink: "eth0".to_string(),
        internet_reachable: true,
        data_cap_state: "ok".to_string(),
    };
    br.reconcile_uplink(Some(&snap));
    let hb = br.build_heartbeat(1).unwrap();
    let t = hb.telemetry.unwrap();
    assert_eq!(t["armed"], true);
    assert_eq!(t["mode"], "GUIDED");
    assert_eq!(t["lat"], 12.97);
    assert_eq!(t["lon"], 77.59);
    assert_eq!(t["alt_rel"], 30.0);
    assert_eq!(t["battery_voltage"], 16.2);
    assert_eq!(t["battery_remaining"], 88);
}

#[test]
fn every_fast_relay_exit_waits_the_same_fixed_interval_with_no_cap() {
    let mut br = bridge();
    // Simulate a reachable uplink + an "up" relay.
    let snap = UplinkSnapshot {
        active_uplink: "eth0".to_string(),
        internet_reachable: true,
        data_cap_state: "ok".to_string(),
    };
    br.reconcile_uplink(Some(&snap));
    br.mqtt_connected = true;
    br.relay_started_at = Some(std::time::Instant::now());

    // The relay exits almost immediately (a flapping IPC socket): the
    // respawn is deferred by exactly one interval.
    let mut now = std::time::Instant::now();
    let delay = br.on_relay_exit(Duration::from_millis(5), now);
    assert_eq!(delay, RELAY_RETRY_INTERVAL);
    assert!(!br.mqtt_connected, "a dead relay is not connected");
    assert!(
        !br.relay_spawn_allowed(now),
        "a respawn is held off until the scheduled instant"
    );
    // The schedule clears once the interval has elapsed.
    assert!(br.relay_spawn_allowed(now + RELAY_RETRY_INTERVAL));

    // And it NEVER grows, however long the seam stays broken: a broker that
    // rejects for the first minutes of a cold boot must not push the node
    // out of the fleet view for minutes after the uplink recovers.
    for attempt in 0..60 {
        now += RELAY_RETRY_INTERVAL;
        assert_eq!(
            br.on_relay_exit(Duration::from_millis(5), now),
            RELAY_RETRY_INTERVAL,
            "attempt {attempt} must wait the same fixed interval"
        );
        assert!(
            br.relay_spawn_allowed(now + RELAY_RETRY_INTERVAL),
            "attempt {attempt} must be retried, not capped out"
        );
    }
}

#[test]
fn a_healthy_relay_run_clears_the_retry_hold() {
    let mut br = bridge();
    let t0 = std::time::Instant::now();
    br.on_relay_exit(Duration::from_millis(1), t0);
    br.on_relay_exit(Duration::from_millis(1), t0);
    assert!(
        !br.relay_spawn_allowed(t0),
        "a fast exit holds the respawn off"
    );

    // A run that lasted past the healthy threshold clears the retry hold so
    // the relay comes straight back.
    let delay = br.on_relay_exit(RELAY_HEALTHY_AFTER + Duration::from_secs(1), t0);
    assert_eq!(delay, Duration::ZERO);
    assert!(
        br.relay_spawn_allowed(t0),
        "no retry hold after a clean run"
    );
    assert!(!br.mqtt_connected);
}

#[test]
fn relay_spawn_allowed_with_no_schedule_is_true() {
    let br = bridge();
    // A fresh bridge has no pending retry, so the first spawn is allowed.
    assert!(br.relay_spawn_allowed(std::time::Instant::now()));
}

#[test]
fn snapshot_reads_the_active_flag_file_with_cap_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("uplink-active");
    // Absent → None (idle).
    assert!(UplinkSnapshot::read(&path).is_none());
    // The ados-net writer's body shape, including the additive cap field.
    std::fs::write(
            &path,
            r#"{"active_uplink":"wwan0","internet_reachable":true,"timestamp_ms":1700000000000,"data_cap_state":"warn_80"}"#,
        )
        .unwrap();
    let s = UplinkSnapshot::read(&path).unwrap();
    assert_eq!(s.active_uplink, "wwan0");
    assert!(s.internet_reachable);
    assert_eq!(s.data_cap_state, "warn_80");
    // A legacy body without the cap field defaults to "ok".
    std::fs::write(
        &path,
        r#"{"active_uplink":"eth0","internet_reachable":true,"timestamp_ms":1}"#,
    )
    .unwrap();
    let s = UplinkSnapshot::read(&path).unwrap();
    assert_eq!(s.data_cap_state, "ok");
}

/// Publish a connection flag onto a bridge as if a relay run just dialed,
/// returning the atomic the (fake) transport event loop would drive. The
/// flag starts `false` exactly like a freshly-dialed rumqttc transport that
/// has not yet seen a ConnAck.
fn arm_relay_flag(br: &mut CloudRelayBridge) -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    let (tx, rx) = watch::channel::<Option<Arc<AtomicBool>>>(Some(flag.clone()));
    // Keep the sender alive for the channel's lifetime (the real relay task
    // holds it); leaking it here mirrors that ownership in a unit test.
    std::mem::forget(tx);
    br.relay_conn_rx = Some(rx);
    flag
}

#[test]
fn dialing_relay_does_not_report_connected_until_the_broker_acks() {
    // The core connect-lie fix: a relay that has spawned and is DIALING a
    // down / never-acking broker must NOT report mqtt_connected:true. The
    // transport's confirmed-connection flag stays false (no ConnAck), so the
    // bridge keeps mqtt_connected false.
    let mut br = bridge();
    let flag = arm_relay_flag(&mut br); // flag == false (broker not acked)

    br.refresh_mqtt_connected();
    assert!(
        !br.mqtt_connected,
        "a dialing relay with no broker ConnAck is not connected"
    );

    // The broker finally accepts the session (ConnAck → flag true): now the
    // bridge reports connected.
    flag.store(true, Ordering::Release);
    br.refresh_mqtt_connected();
    assert!(br.mqtt_connected, "a confirmed ConnAck reports connected");

    // The broker drops the session (Disconnect/poll-error → flag false):
    // the bridge stops reporting connected on the next refresh.
    flag.store(false, Ordering::Release);
    br.refresh_mqtt_connected();
    assert!(
        !br.mqtt_connected,
        "a dropped broker session is no longer connected"
    );
}

#[test]
fn no_relay_flag_means_not_connected() {
    // With no relay run at all there is no flag; the link is down.
    let mut br = bridge();
    assert!(br.relay_conn_rx.is_none());
    br.refresh_mqtt_connected();
    assert!(!br.mqtt_connected);
}

#[test]
fn mark_relay_down_clears_a_confirmed_connection() {
    // A previously-confirmed connection must be cleared on teardown/reap so
    // the next heartbeat cannot report a stale broker session.
    let mut br = bridge();
    let flag = arm_relay_flag(&mut br);
    flag.store(true, Ordering::Release);
    br.refresh_mqtt_connected();
    assert!(br.mqtt_connected, "confirmed connection before teardown");

    br.mark_relay_down();
    assert!(!br.mqtt_connected, "teardown clears the connection");
    assert!(br.relay_connected_flag.is_none());
    assert!(br.relay_conn_rx.is_none());
    // Even if the (now-detached) flag is still true, a refresh stays down:
    // the bridge no longer holds a handle to it.
    flag.store(true, Ordering::Release);
    br.refresh_mqtt_connected();
    assert!(
        !br.mqtt_connected,
        "a detached flag cannot revive the report"
    );
}

#[test]
fn relay_exit_reports_disconnected_even_if_the_stale_flag_lingers() {
    // A relay task that exits (fast or clean) must report disconnected. The
    // transport's flag may briefly linger true after the task ends, but the
    // bridge drops its handle on exit, so the heartbeat goes false.
    let mut br = bridge();
    let flag = arm_relay_flag(&mut br);
    flag.store(true, Ordering::Release);
    br.refresh_mqtt_connected();
    assert!(br.mqtt_connected);

    // The relay task is reaped; the flag is still (stale) true.
    let now = std::time::Instant::now();
    br.on_relay_exit(Duration::from_millis(5), now);
    assert!(!br.mqtt_connected, "a reaped relay is not connected");
    // A subsequent refresh cannot resurrect the report from the stale flag.
    br.refresh_mqtt_connected();
    assert!(!br.mqtt_connected);
}

#[test]
fn heartbeat_reports_mqtt_disconnected_while_the_broker_is_down() {
    // End to end on the payload: a dialing relay over a down broker yields a
    // heartbeat carrying mqttConnected:false, never a connect-lie.
    let mut br = bridge();
    let snap = UplinkSnapshot {
        active_uplink: "eth0".to_string(),
        internet_reachable: true,
        data_cap_state: "ok".to_string(),
    };
    br.reconcile_uplink(Some(&snap));
    // Relay dialing a down broker: flag present but false (no ConnAck).
    arm_relay_flag(&mut br);
    br.refresh_mqtt_connected();

    let hb = br.build_heartbeat(1).unwrap();
    assert!(!hb.mqtt_connected);
    let wire = serde_json::to_value(&hb).unwrap();
    assert_eq!(wire["mqttConnected"], false);
}
