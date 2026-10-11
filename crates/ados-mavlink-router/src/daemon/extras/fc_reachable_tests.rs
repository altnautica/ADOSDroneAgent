use super::compute_fc_reachable;

#[test]
fn a_live_mavlink_heartbeat_is_reachable() {
    assert!(compute_fc_reachable(true, true, false, "none"));
    // Even a heartbeat with a stale/odd transport_open reading counts —
    // a HEARTBEAT decoding at all is definitive evidence of a live link.
    assert!(compute_fc_reachable(true, false, false, "none"));
}

#[test]
fn an_identified_msp_fc_on_an_open_port_is_reachable_without_a_heartbeat() {
    // The exact case this fix targets: Betaflight/iNav never emit a
    // MAVLink heartbeat, so mavlink_alive stays false forever on an MSP
    // board, yet it is present and drivable over the byte-transparent
    // proxy.
    assert!(compute_fc_reachable(false, true, true, "none"));
}

#[test]
fn an_open_port_with_only_the_msp_detected_hint_is_reachable() {
    // The hint path: transport open, no decoded variant string yet, but
    // the link monitor's own sniff already says "this looks like MSP".
    assert!(compute_fc_reachable(false, true, false, "msp_detected"));
}

#[test]
fn an_open_port_with_neither_a_heartbeat_nor_msp_evidence_is_not_reachable() {
    // A port that is open but silent, with nothing identifying an FC
    // behind it, must read as absent — this is the "port open, no
    // MAVLink" amber state, not a connected one.
    assert!(!compute_fc_reachable(false, true, false, "none"));
    assert!(!compute_fc_reachable(false, true, false, "no_heartbeat"));
}

#[test]
fn a_closed_transport_is_never_reachable_regardless_of_stale_hints() {
    assert!(!compute_fc_reachable(false, false, true, "msp_detected"));
    assert!(!compute_fc_reachable(false, false, false, "none"));
}
