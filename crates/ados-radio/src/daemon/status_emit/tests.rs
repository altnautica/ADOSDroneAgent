use super::*;

fn key(state: &str, channel: u8, readback: Option<f32>) -> StatusKey {
    StatusKey {
        state: state.to_string(),
        actual_channel: channel,
        operating_channel: channel,
        reg_domain: Some("US".to_string()),
        reg_verified: true,
        tx_power_dbm: 5,
        tx_power_readback_dbm: readback,
    }
}

#[test]
fn status_emit_dedup() {
    let t0 = Instant::now();
    let mut gate = StatusEmitGate::new();
    // First tick always emits.
    assert!(gate.should_emit(key("connected", 149, Some(5.0)), t0));
    // Unchanged 2 s later: suppressed.
    assert!(!gate.should_emit(
        key("connected", 149, Some(5.0)),
        t0 + Duration::from_secs(2)
    ));
    // A channel change emits at once.
    assert!(gate.should_emit(
        key("connected", 157, Some(5.0)),
        t0 + Duration::from_secs(4)
    ));
    // A readback change emits at once.
    assert!(gate.should_emit(
        key("connected", 157, Some(9.0)),
        t0 + Duration::from_secs(6)
    ));
    // A state change emits at once.
    assert!(gate.should_emit(key("locked", 157, Some(9.0)), t0 + Duration::from_secs(8)));
    // Unchanged until the max gap, then the keep-alive emit.
    assert!(!gate.should_emit(key("locked", 157, Some(9.0)), t0 + Duration::from_secs(14)));
    assert!(gate.should_emit(key("locked", 157, Some(9.0)), t0 + Duration::from_secs(16)));
}
