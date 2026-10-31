use super::*;

#[test]
fn static_config_is_read_synchronously() {
    let ipc = Arc::new(PluginIpcClient::new(
        "com.example.demo",
        "tok",
        "/tmp/x.sock",
    ));
    let mut cfg = BTreeMap::new();
    cfg.insert("palette".to_string(), Value::from("ironbow"));
    let ctx = PluginContext::new(ipc, "1.0.0", "agent-1", None, cfg);
    assert_eq!(
        ctx.config.static_get("palette"),
        Some(&Value::from("ironbow"))
    );
    assert!(ctx.config.static_get("missing").is_none());
    // peripherals is the same client as peripheral_manager.
    assert_eq!(ctx.plugin_id, "com.example.demo");
    assert_eq!(ctx.agent_id, "agent-1");
    assert_eq!(ctx.plugin_version, "1.0.0");
}

/// `send_sticks` must apply the codec scaling and AETR order. The bug it
/// fixes is a control loop handing `send_raw_rc` normalized sticks, where a
/// centre stick (`0.0`) would land at PWM `0` — off the bottom of the range —
/// instead of `1500`. This asserts the scaling is actually applied.
#[test]
fn send_sticks_scales_and_orders_aetr() {
    // Centre sticks + idle throttle: NOT zeros. AETR = [roll, pitch, thr, yaw].
    assert_eq!(
        sticks_to_channels(0.0, 0.0, 0.0, 0.0),
        [1500, 1500, 1000, 1500]
    );
    // Full deflection each axis + full throttle, exercising the ordering.
    assert_eq!(
        sticks_to_channels(1.0, -1.0, 1.0, 1.0),
        [2000, 1000, 2000, 2000]
    );
    // Out-of-range clamps rather than wrapping.
    assert_eq!(
        sticks_to_channels(5.0, -5.0, 0.0, 2.0),
        [2000, 1000, 2000, 1500]
    );
}
