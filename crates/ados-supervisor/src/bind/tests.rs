use super::*;

#[test]
fn bind_state_sidecar_version_matches_registry() {
    // The per-file const and the sidecar registry are the two sources of
    // truth for this sidecar's schema version; a drift is caught here.
    assert_eq!(
        BIND_STATE_SIDECAR_VERSION,
        ados_protocol::contracts::sidecar_version("bind-state").unwrap()
    );
}

#[test]
fn role_round_trips_and_maps_units() {
    assert_eq!(BindRole::parse("drone"), Some(BindRole::Drone));
    assert_eq!(BindRole::parse("gs"), Some(BindRole::Gs));
    assert_eq!(BindRole::parse("ground_station"), Some(BindRole::Gs));
    assert_eq!(BindRole::parse("nope"), None);
    assert_eq!(BindRole::Drone.as_str(), "drone");
    assert_eq!(BindRole::Drone.normal_unit(), "ados-wfb.service");
    assert_eq!(BindRole::Gs.normal_unit(), "ados-wfb-rx.service");
    assert_eq!(
        BindRole::Drone.bind_unit(),
        "wifibroadcast@drone_bind.service"
    );
    assert_eq!(BindRole::Drone.bind_iface(), "drone-bind");
    assert_eq!(BindRole::Drone.key_path(), "/etc/ados/wfb/tx.key");
    assert_eq!(BindRole::Gs.key_path(), "/etc/ados/wfb/rx.key");
    assert_eq!(BindRole::Drone.upstream_key(), "/etc/drone.key");
}

#[test]
fn bind_timeouts_surface_a_wedge_fast() {
    assert_eq!(TUNNEL_WAIT_TIMEOUT.as_secs(), 30);
    // Short windows: a wedged bind frees the single-flight guard in ~2 min,
    // not ~30, so a stuck rendezvous self-clears and auto-pair retries tightly.
    assert_eq!(WAITING_PEER_WATCHDOG.as_secs(), 120);
    assert_eq!(KEY_TRANSFER_TIMEOUT.as_secs(), 90);
    // The reclaim wait must be shorter than the watchdog, or a stale guard
    // could never be reclaimed before the wedged session frees it anyway.
    assert!(LOCK_RECLAIM_TIMEOUT < WAITING_PEER_WATCHDOG);
    assert_eq!(RESTART_TIMEOUT.as_secs(), 60);
    assert_eq!(BIND_TCP_PORT, 5555);
}
