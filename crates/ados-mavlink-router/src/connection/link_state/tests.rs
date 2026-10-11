use super::*;

#[test]
fn only_a_v2_frame_with_the_signed_flag_is_signed() {
    assert!(frame_is_signed(&[0xFD, 9, 0x01, 0]));
    assert!(!frame_is_signed(&[0xFD, 9, 0x00, 0]));
    // MAVLink 1 has no signing.
    assert!(!frame_is_signed(&[0xFE, 9, 0x01, 0]));
    assert!(!frame_is_signed(&[0xFD]));
}

#[test]
fn only_a_cdc_acm_node_is_the_usb_link() {
    assert!(device_is_usb("/dev/ttyACM0"));
    assert!(!device_is_usb("/dev/ttyAMA0"));
    assert!(!device_is_usb("/dev/ttyUSB0"));
    assert!(!device_is_usb("/dev/ttyS1"));
}

#[cfg(unix)]
#[tokio::test]
async fn fc_device_canonical_in_extras() {
    // A symlinked device label (the way `/dev/serial0` points at the real
    // UART) resolves to its target, which is what the extra publishes.
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("ttyAMA0");
    std::fs::write(&target, b"").unwrap();
    let link = dir.path().join("serial0");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let resolved = canonical_fc_device(link.to_str().unwrap()).await;
    let expected = std::fs::canonicalize(&target).unwrap();
    assert_eq!(resolved, Some(expected.to_string_lossy().into_owned()));
}

fn conn() -> std::sync::Arc<FcConnection> {
    let dir = std::env::temp_dir().join("ados-link-state-test-params.json");
    FcConnection::new(
        crate::config::MavlinkConfig::default(),
        std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::state::VehicleState::default(),
        )),
        std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::param_cache::ParamCache::new(dir),
        )),
    )
}

#[tokio::test]
async fn a_signing_fc_on_a_uart_flags_the_unsigned_command_risk() {
    let c = conn();
    // No open link: unmeasured.
    assert_eq!(c.agent_commands_unsigned_rejected_risk().await, None);
    c.connected.store(true, Ordering::Relaxed);
    *c.fc_device.lock().await = Some("/dev/ttyAMA0".into());
    c.note_vehicle_frame(&[0xFD, 9, 0x00, 0]);
    assert_eq!(c.agent_commands_unsigned_rejected_risk().await, Some(false));
    c.note_vehicle_frame(&[0xFD, 9, 0x01, 0]);
    assert_eq!(c.agent_commands_unsigned_rejected_risk().await, Some(true));
    // The FC's own USB port is exempt from enforcement.
    *c.fc_device.lock().await = Some("/dev/ttyACM0".into());
    assert_eq!(c.agent_commands_unsigned_rejected_risk().await, Some(false));
    // A network source has no device to judge.
    *c.fc_device.lock().await = None;
    assert_eq!(c.agent_commands_unsigned_rejected_risk().await, None);
}

#[tokio::test]
async fn arbitration_inactive_on_drone_gates_nothing() {
    let c = conn();
    assert_eq!(c.injector_arbitration_state(), "off");
    let claim = InjectorClaim {
        client_id: "ai-mission".into(),
        ticket: None,
    };
    assert!(!c.injector_refused_now(&claim), "no gate, nothing refused");
    // No sidecar fed: the swarm layer keeps its own gates.
    assert!(!c.swarm_refused_by_pic());
}
