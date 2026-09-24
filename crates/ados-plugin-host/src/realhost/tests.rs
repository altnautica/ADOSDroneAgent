use super::*;

/// A whole v1 frame of the dialect message `msgid`, default fields, with a
/// real checksum. The gate classifies the WHOLE buffer and refuses anything
/// a flight controller would not consume whole, so these fixtures are
/// genuine frames rather than header stubs.
fn v1_frame(msgid: u8) -> Vec<u8> {
    use ados_protocol::mavlink::{MavHeader, MavMessage, Message};
    let msg = MavMessage::default_message_from_id(u32::from(msgid)).expect("dialect id");
    ados_protocol::mavlink::serialize_v1(MavHeader::default(), &msg).expect("serialize")
}

/// A whole v2 frame of the dialect message `msgid`, default fields.
fn v2_frame(msgid: u32) -> Vec<u8> {
    use ados_protocol::mavlink::{MavHeader, MavMessage, Message};
    let msg = MavMessage::default_message_from_id(msgid).expect("dialect id");
    ados_protocol::mavlink::serialize_v2(MavHeader::default(), &msg).expect("serialize")
}

/// A whole v2 frame of the dialect message `msgid`, stamped with source
/// component `component_id`.
fn v2_frame_from(component_id: u8, msgid: u32) -> Vec<u8> {
    use ados_protocol::mavlink::{MavHeader, MavMessage, Message};
    let msg = MavMessage::default_message_from_id(msgid).expect("dialect id");
    let header = MavHeader {
        system_id: 1,
        component_id,
        sequence: 0,
    };
    ados_protocol::mavlink::serialize_v2(header, &msg).expect("serialize")
}

fn caps(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn map(entries: &[(&str, Value)]) -> Value {
    Value::Map(
        entries
            .iter()
            .map(|(k, v)| (Value::from(*k), v.clone()))
            .collect(),
    )
}

fn err_body(r: Result<HostResult, HostError>) -> String {
    match r {
        Err(e) => e.body(),
        Ok(v) => panic!("expected Err, got Ok({v:?})"),
    }
}

fn ok_map(r: Result<HostResult, HostError>) -> Vec<(Value, Value)> {
    match r {
        Ok(Value::Map(m)) => m,
        other => panic!("expected Ok(map), got {other:?}"),
    }
}

fn field<'a>(m: &'a [(Value, Value)], key: &str) -> Option<&'a Value> {
    m.iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}

/// A host that has observed an autopilot heartbeat from system 1,
/// component 1, so a command without explicit targets has somewhere to go.
fn host_with_fc() -> RealHost {
    let host = RealHost::new();
    host.fc_identity().set(1, 1);
    host
}

// ---- unimplemented-method / ungrantable-cap invariants ----------

/// The method name carried in a `not_implemented` result, if it is one.
fn not_implemented_method(r: &Result<HostResult, HostError>) -> Option<String> {
    let Ok(Value::Map(m)) = r else {
        return None;
    };
    let is_ni = m
        .iter()
        .find(|(k, _)| k.as_str() == Some("error"))
        .and_then(|(_, v)| v.as_str())
        == Some("not_implemented");
    if !is_ni {
        return None;
    }
    m.iter()
        .find(|(k, _)| k.as_str() == Some("method"))
        .and_then(|(_, v)| v.as_str())
        .map(str::to_string)
}

#[test]
fn all_dispatch_methods_is_exhaustive() {
    // The local ALL_DISPATCH_METHODS list must cover every generated
    // PLUGIN->HOST method and carry no extras, so ungrantable_caps() reasons
    // over the full set. Host->plugin methods (tool.invoke) are excluded —
    // the host issues them, it does not dispatch them from a plugin request.
    use crate::dispatch::HOST_TO_PLUGIN_METHODS;
    use ados_protocol::dispatch::DISPATCH_METHODS;
    assert_eq!(
        ALL_DISPATCH_METHODS.len(),
        DISPATCH_METHODS.len() - HOST_TO_PLUGIN_METHODS.len()
    );
    for row in DISPATCH_METHODS {
        if HOST_TO_PLUGIN_METHODS.contains(&row.method) {
            continue;
        }
        assert!(
            ALL_DISPATCH_METHODS
                .iter()
                .any(|m| m.wire_name() == row.method),
            "generated method {} missing from ALL_DISPATCH_METHODS",
            row.method
        );
    }
}

#[test]
fn unimplemented_methods_match_reality() {
    // Every method declared unimplemented must actually return the
    // not_implemented shape (with its own method name) from a freshly-built
    // host, with no MAVLink / vision client wired. The async vision methods
    // are exercised in their own test (they need an executor); the
    // synchronous methods are checked directly here.
    let host = RealHost::new();
    let empty = Value::Map(vec![]);
    for method in RealHost::UNIMPLEMENTED_HOST_METHODS {
        use crate::dispatch::Method;
        let r = match method {
            Method::MissionRead => host.mission_read("p", &empty),
            Method::MissionWrite => host.mission_write("p", &empty),
            Method::RecordingStart => host.recording_start("p", &empty),
            Method::RecordingStop => host.recording_stop("p", &empty),
            Method::CameraClaim => host.camera_claim("p", &empty),
            Method::CameraRelease => host.camera_release("p", &empty),
            Method::CameraGetFrame => host.camera_get_frame("p", &empty),
            Method::PeripheralRegisterDriver => {
                host.peripheral_register_driver("p", &empty, &BTreeSet::new())
            }
            Method::PeripheralUnregisterDriver => host.peripheral_unregister_driver("p", &empty),
            other => panic!("unexpected method in the unimplemented list: {other:?}"),
        };
        assert_eq!(
            not_implemented_method(&r).as_deref(),
            Some(method.wire_name()),
            "{} must return not_implemented from a fresh host",
            method.wire_name()
        );
    }
}

#[test]
fn implemented_methods_are_not_not_implemented() {
    // A representative set of WIRED host methods must NOT report
    // not_implemented even on a fresh host: telemetry.extend validates and
    // succeeds, and config.get reads the store. If one of these regressed into
    // a stub, this would catch it and the ungrantable set would need updating.
    let host = RealHost::new();
    let extend = map(&[("channel", Value::from("c")), ("data", map(&[]))]);
    assert!(not_implemented_method(&host.telemetry_extend("p", &extend)).is_none());
    let cfg = map(&[("key", Value::from("k"))]);
    assert!(not_implemented_method(&host.config_get("p", &cfg)).is_none());
}

#[test]
fn ungrantable_caps_are_the_dead_capabilities() {
    // The caps that gate only the unimplemented methods (and the driver-kind
    // caps checked inside the unimplemented driver registration), and nothing
    // a wired surface needs.
    let ungrantable = RealHost::ungrantable_caps();
    let expected: BTreeSet<String> = [
        "mission.read",
        "mission.write",
        "recording.write",
        "sensor.camera.register",
        "sensor.depth.register",
        "sensor.imu.register",
        "sensor.lidar.register",
        "sensor.payload.register",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(ungrantable, expected);
    // A cap that gates a wired method must never be refused: mavlink.read
    // gates mavlink.subscribe, which is served via the stream method.
    assert!(!ungrantable.contains("mavlink.read"));
    assert!(!ungrantable.contains("vision.frame.read"));
}

#[tokio::test]
async fn vision_methods_are_not_in_the_unimplemented_set() {
    // The vision request methods return not_implemented only on a host with no
    // vision client wired (a deployment state, like an unwired mavlink.send),
    // so their caps are NOT permanently ungrantable. Confirm they are absent
    // from the unimplemented list even though a bare host does return
    // not_implemented for them.
    use crate::dispatch::Method;
    assert!(!RealHost::UNIMPLEMENTED_HOST_METHODS.contains(&Method::VisionRegisterModel));
    assert!(!RealHost::UNIMPLEMENTED_HOST_METHODS.contains(&Method::VisionInfer));
    assert!(!RealHost::UNIMPLEMENTED_HOST_METHODS.contains(&Method::VisionPublishDetection));
    // And a bare host does degrade them to not_implemented, which is why they
    // must be excluded by deployment, not by capability.
    let host = RealHost::new();
    let empty = Value::Map(vec![]);
    assert_eq!(
        not_implemented_method(&host.vision_infer("p", &empty).await).as_deref(),
        Some("vision.infer")
    );
}

// ---- ComponentRegistrar -----------------------------------------

#[test]
fn component_cross_plugin_collision_uses_exact_message() {
    let mut reg = ComponentRegistrar::default();
    reg.register("a", 5, "vio", 0).unwrap();
    let err = reg.register("b", 5, "vio", 0).unwrap_err();
    assert_eq!(err, "component_id 5 already reserved by a");
    // Same plugin re-registering its own id is fine.
    assert!(reg.register("a", 5, "vio", 0).is_ok());
}

#[test]
fn component_release_drops_both_indexes() {
    let mut reg = ComponentRegistrar::default();
    reg.register("a", 5, "vio", 0).unwrap();
    assert!(reg.is_registered("a", 5));
    reg.release_session("a", 0);
    assert!(!reg.is_registered("a", 5));
    // The component id is freed for another plugin.
    assert!(reg.register("b", 5, "vio", 0).is_ok());
}

// ---- session-scoped release ---------------------------------------

#[tokio::test]
async fn an_ending_session_keeps_what_a_newer_session_renewed() {
    // The runner reconnected and the new session re-reserved the VIO id before
    // the old connection's teardown ran; the teardown must not release it.
    let host = RealHost::new();
    let vio = map(&[
        ("kind", Value::from("vio")),
        ("component_id", Value::Integer(197.into())),
    ]);
    let cap = caps(&["mavlink.component.vio"]);
    let old = host.begin_session("p");
    host.mavlink_register_component("p", &vio, &cap).unwrap();
    let new = host.begin_session("p");
    host.mavlink_register_component("p", &vio, &cap).unwrap();
    host.release_session("p", old).await;
    assert!(host.components.lock().unwrap().is_registered("p", 197));
    host.release_session("p", new).await;
    assert!(!host.components.lock().unwrap().is_registered("p", 197));
}

// ---- ConfigStore ------------------------------------------------

#[test]
fn config_drone_scope_shadows_global_and_degrades_when_unbound() {
    let mut cfg = ConfigStore::default();
    cfg.set("p", "k", Value::from("global-v"), "global", "")
        .unwrap();
    cfg.set("p", "k", Value::from("drone-v"), "drone", "agent-1")
        .unwrap();
    // With agent bound, drone scope wins.
    assert_eq!(
        cfg.get("p", "k", "agent-1", Value::Nil).as_str(),
        Some("drone-v")
    );
    // Without an agent, falls back to global.
    assert_eq!(cfg.get("p", "k", "", Value::Nil).as_str(), Some("global-v"));
    // drone scope with no agent degrades to global.
    cfg.set("p", "g", Value::from("via-degrade"), "drone", "")
        .unwrap();
    assert_eq!(
        cfg.get("p", "g", "", Value::Nil).as_str(),
        Some("via-degrade")
    );
}

#[test]
fn config_missing_returns_default() {
    let cfg = ConfigStore::default();
    assert_eq!(
        cfg.get("p", "absent", "agent-1", Value::from("fallback"))
            .as_str(),
        Some("fallback")
    );
}

#[test]
fn config_stored_nil_shadows_global_like_the_sentinel() {
    // A value explicitly set to nil at drone scope must shadow a global
    // value and the request default — matching the _MISSING sentinel, which
    // treats a stored None as present.
    let mut cfg = ConfigStore::default();
    cfg.set("p", "k", Value::from("global-v"), "global", "")
        .unwrap();
    cfg.set("p", "k", Value::Nil, "drone", "agent-1").unwrap();
    let got = cfg.get("p", "k", "agent-1", Value::from("default-v"));
    assert!(matches!(got, Value::Nil));
}

// ---- mavlink_msg_id ---------------------------------------------

#[test]
fn mavlink_msg_id_v2_and_v1_and_short() {
    // v2: STX 0xFD, msgid little-endian 24-bit at bytes 7..10. id = 331.
    let mut v2 = vec![0xFD, 0, 0, 0, 0, 0, 0];
    v2.extend_from_slice(&[331u32.to_le_bytes()[0], 331u32.to_le_bytes()[1], 0]);
    assert_eq!(mavlink_msg_id(&v2), Some(331));
    // v1: STX 0xFE, msgid byte 5.
    let v1 = vec![0xFE, 0, 0, 0, 0, 102];
    assert_eq!(mavlink_msg_id(&v1), Some(102));
    // too short -> None.
    assert_eq!(mavlink_msg_id(&[0xFD, 0]), None);
    assert_eq!(mavlink_msg_id(&[]), None);
}

// ---- inline cap gates (now applied inside the handlers) ----------

#[test]
fn pose_inject_gate_demands_estimator_cap() {
    // A v2 ODOMETRY (331) send without estimator.pose.inject is denied. The
    // gate runs inside the handler, after msg_bytes validation.
    let host = RealHost::new();
    let args = map(&[("msg_bytes", Value::Binary(v2_frame(331)))]);
    assert_eq!(
        err_body(host.mavlink_send("p", &args, &caps(&[]))),
        "capability_denied: estimator.pose.inject"
    );
}

#[test]
fn a_benign_leading_frame_does_not_speak_for_the_pose_frames_behind_it() {
    // The bypass. `mavlink.send` takes a buffer and the router forwards it
    // whole without parsing, so a plugin holding mavlink.write but not
    // estimator.pose.inject could put one HEARTBEAT in front of any number
    // of ODOMETRY frames: the old gate read the message id at offset 0,
    // saw the benign one, allowed the send, and every pose frame reached
    // the flight controller's state estimator.
    let host = RealHost::new();
    let mut buf = v2_frame(0); // HEARTBEAT
    buf.extend_from_slice(&v2_frame(331)); // ODOMETRY
    buf.extend_from_slice(&v2_frame(331));
    let args = map(&[("msg_bytes", Value::Binary(buf))]);
    assert_eq!(
        err_body(host.mavlink_send("p", &args, &caps(&["mavlink.write"]))),
        "capability_denied: estimator.pose.inject"
    );
}

#[test]
fn a_pose_frame_anywhere_in_the_buffer_is_caught() {
    // Not just the second position: the scan covers the whole batch, and a
    // v1 frame mixed in with v2 ones does not desynchronise it, because the
    // walk reads only headers.
    let host = RealHost::new();
    let mut buf = v2_frame(0);
    buf.extend_from_slice(&v1_frame(30)); // ATTITUDE, benign
    buf.extend_from_slice(&v2_frame(0));
    buf.extend_from_slice(&v2_frame(102)); // VISION_POSITION_ESTIMATE, last
    let args = map(&[("msg_bytes", Value::Binary(buf))]);
    assert_eq!(
        err_body(host.mavlink_send("p", &args, &caps(&["mavlink.write"]))),
        "capability_denied: estimator.pose.inject"
    );
}

#[test]
fn the_global_frame_vision_estimate_is_gated_like_its_local_frame_sibling() {
    // GLOBAL_VISION_POSITION_ESTIMATE (101) reaches the same estimator as
    // VISION_POSITION_ESTIMATE (102) — it differs only in the frame the pose
    // is expressed in. Gating one and not the other leaves a plugin holding
    // `mavlink.write` alone able to drive the FC's position solution, which
    // is the whole reason the estimator capability is separate.
    let host = RealHost::new();
    let args = map(&[("msg_bytes", Value::Binary(v2_frame(101)))]);
    assert_eq!(
        err_body(host.mavlink_send("p", &args, &caps(&["mavlink.write"]))),
        "capability_denied: estimator.pose.inject"
    );
    // And it is not a ban: with the capability the send proceeds.
    let m = ok_map(host.mavlink_send(
        "p",
        &args,
        &caps(&["mavlink.write", "estimator.pose.inject"]),
    ));
    assert_eq!(
        field(&m, "error").and_then(Value::as_str),
        Some("not_available")
    );
}

#[test]
fn a_buffer_that_is_not_whole_frames_is_refused_rather_than_guessed() {
    // The splitter stops at the first incomplete frame and returns what was
    // whole, but the router forwards the buffer INCLUDING the remainder.
    // Ignoring the tail would reopen the hole one truncation further along:
    // a pose frame split across two sends is reassembled by the flight
    // controller's own parser, which reads a byte stream and neither knows
    // nor cares where our buffers ended.
    let host = RealHost::new();
    let mut buf = v2_frame(0);
    buf.extend_from_slice(&v2_frame(331)[..6]); // a pose frame, truncated
    let args = map(&[("msg_bytes", Value::Binary(buf))]);
    assert_eq!(
        err_body(host.mavlink_send("p", &args, &caps(&["mavlink.write"]))),
        "msg_bytes is not a run of whole, valid MAVLink frames"
    );
}

#[test]
fn a_granted_plugin_may_still_batch_pose_frames() {
    // The gate must not have become a ban. With the capability granted the
    // same batch passes the check and reaches the router (absent here, so
    // it degrades to not_available rather than erroring).
    let host = RealHost::new();
    let mut buf = v2_frame(0);
    buf.extend_from_slice(&v2_frame(331));
    let args = map(&[("msg_bytes", Value::Binary(buf))]);
    let m = ok_map(host.mavlink_send(
        "p",
        &args,
        &caps(&["mavlink.write", "estimator.pose.inject"]),
    ));
    assert_eq!(
        field(&m, "error").and_then(Value::as_str),
        Some("not_available")
    );
}

/// Whether the scan demands the pose capability, or `None` for a buffer it
/// cannot classify.
fn pose_verdict(buf: &[u8]) -> Option<bool> {
    scan_outbound(buf).map(|s| s.requires_pose_cap)
}

#[test]
fn scan_outbound_classifies_whole_partial_and_clear_buffers() {
    assert_eq!(pose_verdict(&v2_frame(0)), Some(false));
    assert_eq!(pose_verdict(&v2_frame(331)), Some(true));
    assert_eq!(pose_verdict(&v1_frame(30)), Some(false));
    // Every id in the set, so adding one to the constant without adding it
    // to the walk cannot pass unnoticed.
    for id in POSE_INJECT_MSG_IDS {
        assert_eq!(
            pose_verdict(&v2_frame(*id)),
            Some(true),
            "message id {id} must demand the capability"
        );
    }
    assert_eq!(pose_verdict(&[0xFD, 9]), None);
    assert_eq!(pose_verdict(b"not a frame"), None);
}

#[test]
fn scan_outbound_reads_every_frame_header_component() {
    let mut buf = v2_frame_from(42, 0);
    buf.extend_from_slice(&v1_frame(30)); // component 0
    buf.extend_from_slice(&v2_frame_from(197, 0));
    let scan = scan_outbound(&buf).expect("whole frames");
    assert_eq!(
        scan.component_ids.into_iter().collect::<Vec<_>>(),
        vec![0, 42, 197]
    );
}

#[test]
fn estimator_velocity_and_gps_inputs_demand_the_pose_capability() {
    // External-nav velocity and the MAVLink GPS driver's two inputs feed the
    // position solution as directly as a vision pose does.
    for id in [103u32, 232, 113] {
        assert_eq!(
            pose_verdict(&v2_frame(id)),
            Some(true),
            "message id {id} must demand the capability"
        );
    }
}

/// A frame the flight controller would reject partway is a container: its
/// parser drops the outer frame after the incompat byte and rescans the
/// payload, so a pose frame inside it reaches the estimator. Only frames
/// consumed whole are classified by their own id.
#[test]
fn a_pose_frame_hidden_in_a_rejected_outer_frame_is_refused() {
    let inner = v2_frame(331);
    let mut outer = vec![0xFD, inner.len() as u8, 0x02, 0, 0, 1, 1, 0, 0, 0];
    outer.extend_from_slice(&inner);
    outer.extend_from_slice(&[0, 0]);
    assert_eq!(pose_verdict(&outer), None);

    // The same container with no foreign flag still fails: a heartbeat
    // cannot carry that payload, and its checksum does not match.
    outer[2] = 0;
    assert_eq!(pose_verdict(&outer), None);

    // A well-formed frame with a corrupted checksum is refused too.
    let mut bad = v2_frame(0);
    let last = bad.len() - 1;
    bad[last] ^= 0xFF;
    assert_eq!(pose_verdict(&bad), None);
}

/// `config.set` of one `key` = `value` for `plugin`.
async fn set_value(
    host: &RealHost,
    plugin: &str,
    key: &str,
    value: Value,
) -> Result<HostResult, HostError> {
    host.config_set(plugin, &map(&[("key", Value::from(key)), ("value", value)]))
        .await
}

#[tokio::test]
async fn config_set_refuses_an_oversized_value() {
    let host = RealHost::new();
    let err = err_body(
        set_value(
            &host,
            "p",
            "k",
            Value::Binary(vec![0u8; CONFIG_VALUE_MAX_BYTES]),
        )
        .await,
    );
    assert!(err.contains("over the 65536-byte limit"), "{err}");
    // Nothing was stored.
    let got = ok_map(host.config_get("p", &map(&[("key", Value::from("k"))])));
    assert_eq!(field(&got, "value"), Some(&Value::Nil));
    // The on-box control path is bounded the same way.
    assert!(host
        .apply_config_set(
            "p",
            "k",
            Value::Binary(vec![0u8; CONFIG_VALUE_MAX_BYTES]),
            "global"
        )
        .await
        .is_err());
}

#[tokio::test]
async fn config_set_bounds_each_plugin_store_without_touching_others() {
    let host = RealHost::new();
    let chunk = || Value::Binary(vec![0u8; 60 * 1024]);
    for i in 0..17 {
        ok_map(set_value(&host, "p", &format!("k{i}"), chunk()).await);
    }
    let err = err_body(set_value(&host, "p", "k17", chunk()).await);
    assert!(err.contains("over the 1048576-byte limit"), "{err}");
    // Rewriting an existing key replaces its bytes rather than adding them.
    ok_map(set_value(&host, "p", "k0", chunk()).await);
    // Another plugin has its own budget.
    ok_map(set_value(&host, "q", "k0", chunk()).await);
}

#[tokio::test]
async fn config_set_caps_the_key_count_per_plugin() {
    let host = RealHost::new();
    for i in 0..CONFIG_PLUGIN_MAX_KEYS {
        ok_map(set_value(&host, "p", &format!("k{i}"), Value::from(1)).await);
    }
    let err = err_body(set_value(&host, "p", "one-more", Value::from(1)).await);
    assert!(err.contains("keys"), "{err}");
    // Rewriting a held key is not a new key.
    ok_map(set_value(&host, "p", "k0", Value::from(1)).await);
    ok_map(set_value(&host, "q", "k0", Value::from(1)).await);
}

#[test]
fn an_oversized_persisted_config_is_not_loaded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plugin-config.json");
    let f = std::fs::File::create(&path).unwrap();
    f.set_len(CONFIG_FILE_MAX_BYTES + 1).unwrap();
    let host = RealHost::new().with_config_persistence(path);
    let got = ok_map(host.config_get(
        "p",
        &map(&[("key", Value::from("k")), ("default", Value::from("none"))]),
    ));
    assert_eq!(field(&got, "value"), Some(&Value::from("none")));
}

#[test]
fn vio_component_gate_demands_vio_cap() {
    let host = RealHost::new();
    let args = map(&[
        ("msg_bytes", Value::Binary(v1_frame(0))),
        ("component_id", Value::Integer(197.into())),
    ]);
    assert_eq!(
        err_body(host.mavlink_send("p", &args, &caps(&[]))),
        "capability_denied: mavlink.component.vio"
    );
}

#[test]
fn string_component_id_participates_in_vio_gate() {
    // Python int("197") parses a numeric string, so a string component id in
    // the VIO set still triggers the cap gate instead of being skipped.
    let host = RealHost::new();
    let args = map(&[
        ("msg_bytes", Value::Binary(v1_frame(0))),
        ("component_id", Value::from("197")),
    ]);
    assert_eq!(
        err_body(host.mavlink_send("p", &args, &caps(&[]))),
        "capability_denied: mavlink.component.vio"
    );
    // A whitespace-padded numeric string parses too (Python int(" 197 ")).
    let padded = map(&[
        ("msg_bytes", Value::Binary(v1_frame(0))),
        ("component_id", Value::from(" 197 ")),
    ]);
    assert_eq!(
        err_body(host.mavlink_send("p", &padded, &caps(&[]))),
        "capability_denied: mavlink.component.vio"
    );
    // A non-numeric string is the "component_id not integer" error.
    let nonnum = map(&[
        ("msg_bytes", Value::Binary(v1_frame(0))),
        ("component_id", Value::from("abc")),
    ]);
    assert_eq!(
        err_body(host.mavlink_send("p", &nonnum, &caps(&[]))),
        "component_id not integer"
    );
}

#[test]
fn mavlink_send_validates_before_capability_gate() {
    // Ordering parity: a non-bytes msg_bytes AND a VIO component_id without
    // the cap fails validation FIRST (msg_bytes must be bytes), not on the
    // capability gate. Mirrors the Python source order.
    let host = RealHost::new();
    let args = map(&[
        ("msg_bytes", Value::Integer(7.into())),
        ("component_id", Value::Integer(197.into())),
    ]);
    assert_eq!(
        err_body(host.mavlink_send("p", &args, &caps(&[]))),
        "msg_bytes must be bytes"
    );
}

#[test]
fn register_component_gate_uses_kind_cap() {
    let host = RealHost::new();
    let args = map(&[
        ("kind", Value::from("vio")),
        ("component_id", Value::Integer(197.into())),
    ]);
    assert_eq!(
        err_body(host.mavlink_register_component("p", &args, &caps(&[]))),
        "capability_denied: mavlink.component.vio"
    );
    // Granted -> registers.
    let m = ok_map(host.mavlink_register_component("p", &args, &caps(&["mavlink.component.vio"])));
    assert_eq!(field(&m, "registered").and_then(Value::as_bool), Some(true));
}

// ---- host method bodies -----------------------------------------

#[test]
fn telemetry_extend_merges_and_validates() {
    let host = RealHost::new();
    let args = map(&[
        ("channel", Value::from("metrics")),
        ("payload", map(&[("x", Value::Integer(1.into()))])),
    ]);
    let m = ok_map(host.telemetry_extend("p", &args));
    assert_eq!(field(&m, "merged").and_then(Value::as_bool), Some(true));
    assert_eq!(
        field(&m, "channel").and_then(Value::as_str),
        Some("metrics")
    );

    // non-map payload errors.
    let bad = map(&[
        ("channel", Value::from("m")),
        ("payload", Value::from("not-a-map")),
    ]);
    assert_eq!(
        err_body(host.telemetry_extend("p", &bad)),
        "payload must be a mapping"
    );
    // empty channel errors.
    let empty = map(&[("channel", Value::from(""))]);
    assert_eq!(
        err_body(host.telemetry_extend("p", &empty)),
        "channel must be a non-empty string"
    );
}

#[test]
fn mavlink_send_without_router_returns_not_available() {
    let host = RealHost::new();
    let args = map(&[("msg_bytes", Value::Binary(v1_frame(0)))]);
    let m = ok_map(host.mavlink_send("p", &args, &caps(&[])));
    assert_eq!(
        field(&m, "error").and_then(Value::as_str),
        Some("not_available")
    );
    assert_eq!(
        field(&m, "method").and_then(Value::as_str),
        Some("mavlink.send")
    );
}

#[test]
fn mavlink_send_empty_and_wrong_type_error() {
    let host = RealHost::new();
    let empty = map(&[("msg_bytes", Value::Binary(vec![]))]);
    assert_eq!(
        err_body(host.mavlink_send("p", &empty, &caps(&[]))),
        "msg_bytes must be non-empty"
    );
    let wrong = map(&[("msg_bytes", Value::Integer(7.into()))]);
    assert_eq!(
        err_body(host.mavlink_send("p", &wrong, &caps(&[]))),
        "msg_bytes must be bytes"
    );
    // A msgpack string is also rejected (Python accepts only bytes/list).
    let as_str = map(&[("msg_bytes", Value::from("not-bytes"))]);
    assert_eq!(
        err_body(host.mavlink_send("p", &as_str, &caps(&[]))),
        "msg_bytes must be bytes"
    );
}

#[test]
fn mavlink_send_unreserved_component_errors() {
    let host = RealHost::new();
    let args = map(&[
        ("msg_bytes", Value::Binary(v1_frame(0))),
        ("component_id", Value::Integer(42.into())),
    ]);
    assert_eq!(
        err_body(host.mavlink_send("p", &args, &caps(&[]))),
        "component_id 42 not reserved by p; call mavlink.register_component first"
    );
}

#[test]
fn a_frame_stamped_with_a_vio_component_needs_the_capability_and_a_reservation() {
    // The plugin declares nothing: the component is read from the header.
    let host = RealHost::new();
    let args = map(&[("msg_bytes", Value::Binary(v2_frame_from(197, 0)))]);
    assert_eq!(
        err_body(host.mavlink_send("p", &args, &caps(&["mavlink.write"]))),
        "capability_denied: mavlink.component.vio"
    );
    let vio = caps(&["mavlink.write", "mavlink.component.vio"]);
    assert_eq!(
        err_body(host.mavlink_send("p", &args, &vio)),
        "component_id 197 not reserved by p; call mavlink.register_component first"
    );
    host.mavlink_register_component(
        "p",
        &map(&[
            ("kind", Value::from("vio")),
            ("component_id", Value::Integer(197.into())),
        ]),
        &vio,
    )
    .unwrap();
    // Reserved and granted: the send reaches the (absent) router.
    let m = ok_map(host.mavlink_send("p", &args, &vio));
    assert_eq!(
        field(&m, "error").and_then(Value::as_str),
        Some("not_available")
    );
}

#[test]
fn a_frame_stamped_with_another_plugins_component_is_refused() {
    let host = RealHost::new();
    host.mavlink_register_component(
        "a",
        &map(&[
            ("kind", Value::from("camera")),
            ("component_id", Value::Integer(100.into())),
        ]),
        &caps(&["mavlink.component.camera"]),
    )
    .unwrap();
    // A batch whose second frame speaks as plugin a's camera.
    let mut buf = v2_frame_from(0, 0);
    buf.extend_from_slice(&v2_frame_from(100, 0));
    let args = map(&[("msg_bytes", Value::Binary(buf))]);
    assert_eq!(
        err_body(host.mavlink_send("b", &args, &caps(&["mavlink.write"]))),
        "component_id 100 already reserved by a"
    );
    // The holder itself may send as its component.
    let own = map(&[("msg_bytes", Value::Binary(v2_frame_from(100, 0)))]);
    ok_map(host.mavlink_send("a", &own, &caps(&["mavlink.write"])));
}

#[test]
fn a_declared_component_id_must_match_every_frame_header() {
    let host = RealHost::new();
    host.mavlink_register_component(
        "p",
        &map(&[
            ("kind", Value::from("camera")),
            ("component_id", Value::Integer(100.into())),
        ]),
        &caps(&["mavlink.component.camera"]),
    )
    .unwrap();
    let mut buf = v2_frame_from(100, 0);
    buf.extend_from_slice(&v2_frame_from(42, 0));
    let args = map(&[
        ("msg_bytes", Value::Binary(buf)),
        ("component_id", Value::Integer(100.into())),
    ]);
    assert_eq!(
        err_body(host.mavlink_send("p", &args, &caps(&["mavlink.write"]))),
        "component_id 100 does not match the frame header component id 42"
    );
}

#[test]
fn an_unparseable_buffer_is_refused_even_with_every_capability() {
    // A container frame the autopilot drops partway hides the frame inside it
    // from every header gate, so holding the pose capability does not make an
    // unclassifiable buffer sendable.
    let host = RealHost::new();
    let inner = v2_frame_from(197, 0);
    let mut outer = vec![0xFD, inner.len() as u8, 0x02, 0, 0, 1, 1, 0, 0, 0];
    outer.extend_from_slice(&inner);
    outer.extend_from_slice(&[0, 0]);
    let args = map(&[("msg_bytes", Value::Binary(outer))]);
    assert_eq!(
        err_body(host.mavlink_send(
            "p",
            &args,
            &caps(&["mavlink.write", "estimator.pose.inject"])
        )),
        "msg_bytes is not a run of whole, valid MAVLink frames"
    );
}

#[test]
fn register_component_vio_reservation_rule() {
    let host = RealHost::new();
    // A VIO id with a non-vio kind is refused (with the camera cap granted so
    // the reservation rule, not the cap gate, is what fires).
    let args = map(&[
        ("kind", Value::from("camera")),
        ("component_id", Value::Integer(197.into())),
    ]);
    assert_eq!(
        err_body(host.mavlink_register_component("p", &args, &caps(&["mavlink.component.camera"]))),
        "component_id 197 is reserved for kind=vio"
    );
    // The right kind registers and returns the shape.
    let ok = map(&[
        ("kind", Value::from("vio")),
        ("component_id", Value::Integer(197.into())),
    ]);
    let m = ok_map(host.mavlink_register_component("p", &ok, &caps(&["mavlink.component.vio"])));
    assert_eq!(field(&m, "registered").and_then(Value::as_bool), Some(true));
    assert_eq!(field(&m, "component_id").and_then(Value::as_i64), Some(197));
    assert_eq!(field(&m, "kind").and_then(Value::as_str), Some("vio"));
}

#[tokio::test]
async fn config_get_set_round_trip_with_agent_lookup() {
    let host = RealHost::new().with_agent_id_lookup(Box::new(|_pid| "agent-1".to_string()));
    let s = ok_map(set_value(&host, "p", "k", Value::from("v")).await);
    assert_eq!(field(&s, "set").and_then(Value::as_bool), Some(true));
    assert_eq!(field(&s, "scope").and_then(Value::as_str), Some("drone"));
    let get_args = map(&[("key", Value::from("k"))]);
    let g = ok_map(host.config_get("p", &get_args));
    assert_eq!(field(&g, "value").and_then(Value::as_str), Some("v"));
}

#[tokio::test]
async fn config_snapshot_is_what_the_plugin_reads_on_this_drone() {
    use crate::control::ConfigControl;
    let host = RealHost::new().with_agent_id_lookup(Box::new(|_pid| "agent-1".to_string()));
    for (plugin, key, value, scope) in [
        ("p", "mode", Value::from("global-mode"), "global"),
        ("p", "distance", Value::from(10), "global"),
        ("p", "distance", Value::from(25), "drone"),
        ("other", "distance", Value::from(99), "drone"),
    ] {
        host.apply_config_set(plugin, key, value, scope)
            .await
            .unwrap();
    }

    let snap = host.config_snapshot("p").unwrap();
    let snap = snap.as_map().expect("a map");
    // The drone's own value wins over the global one, a global-only key is
    // still there, and another plugin's config never appears.
    assert_eq!(field(snap, "distance").and_then(Value::as_i64), Some(25));
    assert_eq!(
        field(snap, "mode").and_then(Value::as_str),
        Some("global-mode")
    );
    assert_eq!(snap.len(), 2);
}

#[tokio::test]
async fn apply_config_set_is_session_scoped_per_plugin() {
    // The on-box control-socket write (apply_config_set, the GCS skill/
    // settings path) writes ONLY the named plugin's own namespace: a write
    // for plugin "a" is invisible to plugin "b". This is the reason config.*
    // is intentionally session-scoped rather than capability-gated — an
    // off-RPC write can never cross-write another plugin's config.
    let host = RealHost::new().with_agent_id_lookup(Box::new(|_pid| "agent-1".to_string()));
    let scope = host
        .apply_config_set("a", "active", Value::Boolean(true), "drone")
        .await
        .expect("write a");
    assert_eq!(scope, "drone");
    // a sees its own value.
    let g_a = ok_map(host.config_get("a", &map(&[("key", Value::from("active"))])));
    assert_eq!(field(&g_a, "value").and_then(Value::as_bool), Some(true));
    // b does not — its default comes back.
    let g_b = ok_map(host.config_get(
        "b",
        &map(&[
            ("key", Value::from("active")),
            ("default", Value::Boolean(false)),
        ]),
    ));
    assert_eq!(field(&g_b, "value").and_then(Value::as_bool), Some(false));
}

#[tokio::test]
async fn apply_config_set_rejects_a_bad_scope() {
    let host = RealHost::new();
    let err = host
        .apply_config_set("p", "k", Value::from("v"), "nonsense")
        .await
        .unwrap_err();
    assert!(err.contains("scope must be drone or global"));
}

#[tokio::test]
async fn config_value_validated_against_the_manifest_schema() {
    let dir = tempfile::tempdir().unwrap();
    let plugin_dir = dir.path().join("com.example.myplugin");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    std::fs::write(
        plugin_dir.join("manifest.yaml"),
        r#"
id: com.example.myplugin
version: 0.1.0
compatibility:
  ados_version: ">=0.9.0"
gcs:
  entrypoint: index.html
  contributes:
    parameters:
      - key: follow_distance_m
        schema:
          type: number
          minimum: 2
          maximum: 50
"#,
    )
    .unwrap();
    let pd = plugin_dir.clone();
    let host = RealHost::new().with_runtime_lookup(Box::new(move |id| {
        if id == "com.example.myplugin" {
            Some((pd.clone(), std::collections::BTreeSet::new()))
        } else {
            None
        }
    }));

    // In-bounds value passes the schema.
    assert!(host
        .apply_config_set(
            "com.example.myplugin",
            "follow_distance_m",
            Value::from(8.0),
            "drone"
        )
        .await
        .is_ok());
    // Out-of-bounds value (> maximum) is rejected before it is persisted.
    let err = host
        .apply_config_set(
            "com.example.myplugin",
            "follow_distance_m",
            Value::from(999.0),
            "drone",
        )
        .await
        .unwrap_err();
    assert!(err.contains("follow_distance_m"), "{err}");
    // A key with no declared schema is allowed (legacy / schemaless keys).
    assert!(host
        .apply_config_set(
            "com.example.myplugin",
            "no_schema_key",
            Value::from("x"),
            "drone"
        )
        .await
        .is_ok());
    // No runtime lookup at all -> allowed (no schema source).
    assert!(RealHost::new()
        .apply_config_set("p", "k", Value::from(999.0), "drone")
        .await
        .is_ok());

    // The compiled schema is cached per manifest revision, not forever: an
    // updated manifest (a plugin upgrade) takes effect on the next write.
    let manifest = plugin_dir.join("manifest.yaml");
    let upgraded = std::fs::read_to_string(&manifest)
        .unwrap()
        .replace("maximum: 50", "maximum: 1000");
    std::fs::write(&manifest, upgraded).unwrap();
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
    std::fs::File::options()
        .write(true)
        .open(&manifest)
        .unwrap()
        .set_modified(later)
        .unwrap();
    assert!(host
        .apply_config_set(
            "com.example.myplugin",
            "follow_distance_m",
            Value::from(999.0),
            "drone"
        )
        .await
        .is_ok());
}

#[tokio::test]
async fn config_set_validation() {
    let host = RealHost::new();
    // missing value.
    assert_eq!(
        err_body(
            host.config_set("p", &map(&[("key", Value::from("k"))]))
                .await
        ),
        "value missing"
    );
    // bad scope: a truthy value that is neither drone nor global errors.
    let bad = map(&[
        ("key", Value::from("k")),
        ("value", Value::from("v")),
        ("scope", Value::from("fleet")),
    ]);
    assert_eq!(
        err_body(host.config_set("p", &bad).await),
        "scope must be drone or global, got 'fleet'"
    );
    // empty key.
    let nokey = map(&[("key", Value::from("")), ("value", Value::from("v"))]);
    assert_eq!(
        err_body(host.config_set("p", &nokey).await),
        "key must be a non-empty string"
    );
}

#[tokio::test]
async fn config_set_falsy_scope_coerces_to_drone() {
    // Mirrors Python `scope = args.get("scope") or "drone"`: any falsy scope
    // value is accepted and treated as "drone".
    let host = RealHost::new();
    for falsy in [
        Value::Nil,
        Value::from(""),
        Value::Integer(0.into()),
        Value::Boolean(false),
        Value::Array(vec![]),
        Value::Map(vec![]),
    ] {
        let args = map(&[
            ("key", Value::from("k")),
            ("value", Value::from("v")),
            ("scope", falsy.clone()),
        ]);
        let m = ok_map(host.config_set("p", &args).await);
        assert_eq!(
            field(&m, "scope").and_then(Value::as_str),
            Some("drone"),
            "falsy scope {falsy:?} should coerce to drone"
        );
    }
}

#[tokio::test]
async fn config_set_truthy_non_string_scope_errors_with_repr() {
    // A truthy non-string scope (a non-empty array of strings) is neither
    // drone nor global; the error reprs it with single-quoted inner strings.
    let host = RealHost::new();
    let args = map(&[
        ("key", Value::from("k")),
        ("value", Value::from("v")),
        ("scope", Value::Array(vec![Value::from("x")])),
    ]);
    assert_eq!(
        err_body(host.config_set("p", &args).await),
        "scope must be drone or global, got ['x']"
    );
}

#[test]
fn process_spawn_no_lookup_returns_not_available() {
    let host = RealHost::new();
    let args = map(&[("basename", Value::from("ffmpeg"))]);
    let m = ok_map(host.process_spawn("p", &args));
    assert_eq!(
        field(&m, "error").and_then(Value::as_str),
        Some("not_available")
    );
    assert_eq!(
        field(&m, "method").and_then(Value::as_str),
        Some("process.spawn")
    );
}

#[test]
fn process_spawn_unregistered_runtime_returns_reason() {
    let host = RealHost::new().with_runtime_lookup(Box::new(|_pid| None));
    let args = map(&[("basename", Value::from("ffmpeg"))]);
    let m = ok_map(host.process_spawn("p", &args));
    assert_eq!(
        field(&m, "reason").and_then(Value::as_str),
        Some("plugin runtime not registered")
    );
}

#[test]
fn process_spawn_allowlist_hit_and_miss() {
    let host = RealHost::new().with_runtime_lookup(Box::new(|_pid| {
        let mut allow = BTreeSet::new();
        allow.insert("ffmpeg".to_string());
        Some((PathBuf::from("/opt/ados/plugins/p"), allow))
    }));
    // Hit: authorized shape.
    let hit = map(&[
        ("basename", Value::from("ffmpeg")),
        ("args", Value::Array(vec![Value::from("-i")])),
        ("env", map(&[("K", Value::from("V"))])),
    ]);
    let m = ok_map(host.process_spawn("p", &hit));
    assert_eq!(field(&m, "authorized").and_then(Value::as_bool), Some(true));
    assert_eq!(
        field(&m, "install_dir").and_then(Value::as_str),
        Some("/opt/ados/plugins/p")
    );
    assert_eq!(
        field(&m, "basename").and_then(Value::as_str),
        Some("ffmpeg")
    );
    assert!(matches!(field(&m, "args"), Some(Value::Array(_))));
    // Miss: allowlist_violation error.
    let miss = map(&[("basename", Value::from("rm"))]);
    assert_eq!(
        err_body(host.process_spawn("p", &miss)),
        "allowlist_violation: rm"
    );
}

#[tokio::test]
async fn release_plugin_clears_all_but_config() {
    let host = RealHost::new().with_agent_id_lookup(Box::new(|_pid| "agent-1".to_string()));
    // Seed every facade for plugin "p".
    host.mavlink_register_component(
        "p",
        &map(&[
            ("kind", Value::from("vio")),
            ("component_id", Value::Integer(197.into())),
        ]),
        &caps(&["mavlink.component.vio"]),
    )
    .unwrap();
    host.telemetry_extend("p", &map(&[("channel", Value::from("c"))]))
        .unwrap();
    ok_map(set_value(&host, "p", "k", Value::from("v")).await);

    host.release_session("p", 0).await;

    // Component reservations cleared.
    assert!(!host.components.lock().unwrap().is_registered("p", 197));
    // Config survives the release.
    let g = ok_map(host.config_get("p", &map(&[("key", Value::from("k"))])));
    assert_eq!(field(&g, "value").and_then(Value::as_str), Some("v"));
}

#[tokio::test]
async fn drone_scoped_config_isolates_per_agent_with_a_real_lookup() {
    // With a real agent-id lookup wired, a drone-scoped write lands under
    // that agent id and a different agent sees its own (or the default),
    // instead of every drone write collapsing to one global bucket.
    let host = RealHost::new().with_agent_id_lookup(Box::new(|_pid| "drone-A".to_string()));
    host.config_set(
        "p",
        &map(&[
            ("key", Value::from("k")),
            ("value", Value::from("for-A")),
            ("scope", Value::from("drone")),
        ]),
    )
    .await
    .unwrap();
    // The store keyed it under the resolved agent id, not global.
    {
        let cfg = host.config.lock().unwrap();
        assert!(cfg
            .drone
            .contains_key(&("p".to_string(), "drone-A".to_string(), "k".to_string())));
        assert!(cfg.global.is_empty());
    }
    // A host bound to a different drone falls through to its default.
    let host_b = RealHost::new().with_agent_id_lookup(Box::new(|_pid| "drone-B".to_string()));
    let g = ok_map(host_b.config_get(
        "p",
        &map(&[("key", Value::from("k")), ("default", Value::from("dflt"))]),
    ));
    assert_eq!(field(&g, "value").and_then(Value::as_str), Some("dflt"));
}

#[tokio::test]
async fn config_persists_and_reloads_across_a_restart() {
    // A persisted store flushes on set; a fresh store loaded from the same
    // path sees the prior value, proving config survives a plugin-host
    // restart (the in-memory-only store lost it).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plugin-config.json");

    let host = RealHost::new()
        .with_agent_id_lookup(Box::new(|_pid| "drone-A".to_string()))
        .with_config_persistence(path.clone());
    host.config_set(
        "p",
        &map(&[
            ("key", Value::from("k")),
            ("value", Value::from("kept")),
            ("scope", Value::from("drone")),
        ]),
    )
    .await
    .unwrap();
    // A global write too, so both scopes round-trip.
    host.config_set(
        "p",
        &map(&[
            ("key", Value::from("g")),
            ("value", Value::from("global-kept")),
            ("scope", Value::from("global")),
        ]),
    )
    .await
    .unwrap();
    assert!(path.exists(), "config.set must flush the store to disk");

    // A brand-new host loaded from the same path (a restart) sees both.
    let reborn = RealHost::new()
        .with_agent_id_lookup(Box::new(|_pid| "drone-A".to_string()))
        .with_config_persistence(path.clone());
    let drone = ok_map(reborn.config_get("p", &map(&[("key", Value::from("k"))])));
    assert_eq!(field(&drone, "value").and_then(Value::as_str), Some("kept"));
    let global = ok_map(reborn.config_get("p", &map(&[("key", Value::from("g"))])));
    assert_eq!(
        field(&global, "value").and_then(Value::as_str),
        Some("global-kept")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn persisted_config_file_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plugin-config.json");
    let host = RealHost::new().with_config_persistence(path.clone());
    ok_map(set_value(&host, "p", "k", Value::from("v")).await);
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "persisted plugin config must be 0600");
}

#[tokio::test]
async fn an_older_config_snapshot_never_overwrites_a_newer_one() {
    // Two sets race to the blocking pool; whichever write runs last, the file
    // must end up holding the newer store.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plugin-config.json");
    let host = RealHost::new();
    let snapshot = |generation: u64, body: &str| ConfigSnapshot {
        path: path.clone(),
        generation,
        json: body.as_bytes().to_vec(),
    };
    host.persist_config(snapshot(2, "[\"newer\"]")).await;
    host.persist_config(snapshot(1, "[\"older\"]")).await;
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "[\"newer\"]");
}

#[tokio::test]
async fn vision_read_model_returns_the_plugins_resolved_status() {
    use crate::state::{save_state, PluginInstall, PluginSource, PluginStatus};
    // A plugin-state file carrying one plugin's resolved model_status.
    let dir = tempfile::tempdir().unwrap();
    let state_path = dir.path().join("plugin-state.json");
    let install = PluginInstall {
        plugin_id: "com.example.p".into(),
        version: "1.0.0".into(),
        source: PluginSource::Registry,
        source_uri: None,
        signer_id: None,
        manifest_hash: "h".into(),
        status: PluginStatus::Running,
        installed_at: 0,
        enabled_at: None,
        failure_reason: None,
        permissions: Default::default(),
        auto_update: true,
        pinned_version: None,
        last_update_check_at: None,
        last_update_attempt: None,
        model_status: Some(serde_json::json!([
            {"state": "resolved", "model_id": "uav", "runtime": "onnx",
             "path": "/var/ados/models/uav.onnx", "reason": null}
        ])),
        service_status: None,
    };
    save_state(&[install], Some(&state_path)).unwrap();

    let host = RealHost::new().with_state_path(state_path.clone());
    let res = host
        .vision_read_model("com.example.p", &Value::Map(vec![]))
        .await
        .unwrap();
    let m = match res {
        Value::Map(m) => m,
        other => panic!("{other:?}"),
    };
    let models = field(&m, "models")
        .and_then(Value::as_array)
        .expect("models array");
    assert_eq!(models.len(), 1);
    let m0 = match &models[0] {
        Value::Map(m) => m,
        other => panic!("{other:?}"),
    };
    assert_eq!(field(m0, "model_id").and_then(Value::as_str), Some("uav"));
    assert_eq!(
        field(m0, "path").and_then(Value::as_str),
        Some("/var/ados/models/uav.onnx"),
        "the resolved model path must reach the plugin"
    );
    assert_eq!(field(m0, "state").and_then(Value::as_str), Some("resolved"));

    // An unknown plugin (or a plugin whose models are unresolved) yields an
    // empty list, never an error — a caller can poll until it resolves.
    let empty = host
        .vision_read_model("com.example.other", &Value::Map(vec![]))
        .await
        .unwrap();
    let em = match empty {
        Value::Map(m) => m,
        other => panic!("{other:?}"),
    };
    assert!(field(&em, "models")
        .and_then(Value::as_array)
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn vision_methods_proxy_to_a_wired_engine() {
    // With a vision client wired to a fake engine socket, the vision methods
    // proxy to it and return its response instead of the not_implemented
    // shape; while that engine is down they answer a transient error, and
    // without a client at all they stay not_implemented.
    use ados_protocol::frame::{encode_frame, PLUGIN_MAX_FRAME};
    use ados_protocol::ipc::IpcBroadcast;
    use ados_protocol::plugin::{Envelope, PROTOCOL_VERSION};

    // Unwired: returns the not_implemented shape (the slot is None).
    let bare = RealHost::new();
    let res = bare
        .vision_register_model("p", &Value::Map(vec![]))
        .await
        .unwrap();
    let m = match res {
        Value::Map(m) => m,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        field(&m, "error").and_then(Value::as_str),
        Some("not_implemented")
    );

    // Wired, engine not up yet: a transient error, not not_implemented.
    let mut sock = std::env::temp_dir();
    sock.push(format!("ados-realhost-vis-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);
    let client = std::sync::Arc::new(VisionClient::spawn_with_interval(
        sock.clone(),
        std::time::Duration::from_millis(50),
    ));
    let host = RealHost::new().with_vision(client.clone());
    assert_eq!(
        host.vision_infer("p", &Value::Map(vec![])).await,
        Err(HostError::Rpc(
            crate::vision_client::VISION_ENGINE_UNAVAILABLE.to_string()
        ))
    );

    // The engine appears: the same host proxies to it once connected.
    let (server, _inbound) = IpcBroadcast::bind(&sock, 256, false, None).await.unwrap();
    assert!(
        client
            .connected_within(std::time::Duration::from_secs(2))
            .await
    );
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let reply = Envelope {
        version: PROTOCOL_VERSION,
        kind: "response".to_string(),
        method: "response".to_string(),
        capability: String::new(),
        args: Value::Map(vec![(Value::from("registered"), Value::Boolean(true))]),
        request_id: "vis-1".to_string(),
        token: String::new(),
        error: None,
    };
    let body = reply.to_msgpack().unwrap();
    server
        .broadcast(encode_frame(&body, PLUGIN_MAX_FRAME).unwrap().into())
        .await;

    let res = host
        .vision_register_model(
            "p",
            &Value::Map(vec![(Value::from("model"), Value::from("m"))]),
        )
        .await
        .unwrap();
    let m = match res {
        Value::Map(m) => m,
        other => panic!("{other:?}"),
    };
    assert_eq!(field(&m, "registered").and_then(Value::as_bool), Some(true));
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn stubbed_methods_inherit_not_implemented() {
    let host = RealHost::new();
    for (got, name) in [
        (host.mission_read("p", &Value::Map(vec![])), "mission.read"),
        (
            host.mission_write("p", &Value::Map(vec![])),
            "mission.write",
        ),
        (
            host.recording_start("p", &Value::Map(vec![])),
            "recording.start",
        ),
        (
            host.recording_stop("p", &Value::Map(vec![])),
            "recording.stop",
        ),
    ] {
        let m = ok_map(got);
        assert_eq!(
            field(&m, "error").and_then(Value::as_str),
            Some("not_implemented")
        );
        assert_eq!(field(&m, "method").and_then(Value::as_str), Some(name));
    }
}

// ---- display.page.set ------------------------------------------------

#[test]
fn display_page_set_writes_the_sidecar_in_the_shared_shape() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lcd-plugin-page.json");
    let host = RealHost::new().with_display_page_path(path.clone());

    let row = Value::Map(vec![
        (Value::from("label"), Value::from("Temp")),
        (Value::from("value"), Value::from("42 C")),
    ]);
    let zone = Value::Map(vec![
        (Value::from("x"), Value::from(8)),
        (Value::from("y"), Value::from(40)),
        (Value::from("w"), Value::from(100)),
        (Value::from("h"), Value::from(32)),
        (Value::from("key"), Value::from("reset")),
        (Value::from("label"), Value::from("Reset")),
    ]);
    let args = map(&[
        ("title", Value::from("Sensor")),
        ("rows", Value::Array(vec![row])),
        ("zones", Value::Array(vec![zone])),
    ]);

    let m = ok_map(host.display_page_set("p", &args));
    assert_eq!(field(&m, "set").and_then(Value::as_bool), Some(true));
    assert_eq!(field(&m, "rows").and_then(Value::as_i64), Some(1));
    assert_eq!(field(&m, "zones").and_then(Value::as_i64), Some(1));

    // The written JSON matches the shape the display loader reads.
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(v["title"], "Sensor");
    assert_eq!(v["rows"][0]["label"], "Temp");
    assert_eq!(v["rows"][0]["value"], "42 C");
    assert_eq!(v["zones"][0]["x"], 8);
    assert_eq!(v["zones"][0]["key"], "reset");
    assert_eq!(v["zones"][0]["label"], "Reset");

    // No stray tmp left behind.
    let stray = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"));
    assert!(!stray);
}

#[test]
fn display_page_set_is_lenient_and_rejects_a_misshaped_list() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lcd-plugin-page.json");
    let host = RealHost::new().with_display_page_path(path.clone());

    // An empty payload writes an empty page.
    let m = ok_map(host.display_page_set("p", &map(&[])));
    assert_eq!(field(&m, "rows").and_then(Value::as_i64), Some(0));
    assert_eq!(field(&m, "zones").and_then(Value::as_i64), Some(0));
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(v["title"], "");

    // A non-list rows is a clear error.
    assert_eq!(
        err_body(host.display_page_set("p", &map(&[("rows", Value::from("nope"))]))),
        "rows must be a list"
    );
    assert_eq!(
        err_body(host.display_page_set("p", &map(&[("zones", Value::from(3))]))),
        "zones must be a list"
    );
}

#[test]
fn display_page_set_refuses_a_page_past_the_size_caps() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lcd-plugin-page.json");
    let host = RealHost::new().with_display_page_path(path.clone());
    let row = || {
        Value::Map(vec![
            (Value::from("label"), Value::from("L")),
            (Value::from("value"), Value::from("V")),
        ])
    };
    let zone = || Value::Map(vec![(Value::from("key"), Value::from("k"))]);

    // Exactly at every cap is accepted.
    let at_caps = map(&[
        ("title", Value::from("t".repeat(DISPLAY_PAGE_MAX_TEXT))),
        (
            "rows",
            Value::Array((0..DISPLAY_PAGE_MAX_ROWS).map(|_| row()).collect()),
        ),
        (
            "zones",
            Value::Array((0..DISPLAY_PAGE_MAX_ZONES).map(|_| zone()).collect()),
        ),
    ]);
    let m = ok_map(host.display_page_set("p", &at_caps));
    assert_eq!(
        field(&m, "rows").and_then(Value::as_i64),
        Some(DISPLAY_PAGE_MAX_ROWS as i64)
    );
    let accepted = std::fs::read(&path).unwrap();

    // One past any cap is refused, and the page on disk is left as it was.
    let too_many_rows = map(&[(
        "rows",
        Value::Array((0..=DISPLAY_PAGE_MAX_ROWS).map(|_| row()).collect()),
    )]);
    assert_eq!(
        err_body(host.display_page_set("p", &too_many_rows)),
        format!("rows has more than {DISPLAY_PAGE_MAX_ROWS} entries")
    );
    let too_many_zones = map(&[(
        "zones",
        Value::Array((0..=DISPLAY_PAGE_MAX_ZONES).map(|_| zone()).collect()),
    )]);
    assert_eq!(
        err_body(host.display_page_set("p", &too_many_zones)),
        format!("zones has more than {DISPLAY_PAGE_MAX_ZONES} entries")
    );
    let long_value = map(&[(
        "rows",
        Value::Array(vec![Value::Map(vec![(
            Value::from("value"),
            Value::from("v".repeat(DISPLAY_PAGE_MAX_TEXT + 1)),
        )])]),
    )]);
    assert_eq!(
        err_body(host.display_page_set("p", &long_value)),
        format!("row value longer than {DISPLAY_PAGE_MAX_TEXT} characters")
    );
    assert_eq!(std::fs::read(&path).unwrap(), accepted);
}

#[test]
fn display_page_set_is_gated_on_the_display_capability() {
    use crate::dispatch::{gate, Gate, Method};
    // The handler itself does not gate; the dispatch loop does. An ungranted
    // caller never reaches the handler.
    assert_eq!(
        gate("display.page.set", false, &caps(&[])),
        Gate::CapabilityDenied("capability_denied: display.oled.page".to_string())
    );
    assert_eq!(
        gate("display.page.set", false, &caps(&["display.oled.page"])),
        Gate::Allow(Method::DisplayPageSet)
    );
}

// ---- gpio.output.set / gpio.buzzer.beep ------------------------------

/// Run a one-shot stub on a unix socket that captures the request line and
/// replies with `reply`. Returns the captured request once a client connects.
#[cfg(unix)]
fn gpio_stub(path: std::path::PathBuf, reply: &'static str) -> std::thread::JoinHandle<String> {
    gpio_stub_after(path, reply, std::time::Duration::ZERO)
}

/// [`gpio_stub`] that holds its reply for `delay` after reading the request,
/// like a service that applies the change before it answers.
#[cfg(unix)]
fn gpio_stub_after(
    path: std::path::PathBuf,
    reply: &'static str,
    delay: std::time::Duration,
) -> std::thread::JoinHandle<String> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    let listener = UnixListener::bind(&path).expect("bind stub socket");
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buf = Vec::new();
        let mut chunk = [0u8; 256];
        loop {
            let n = stream.read(&mut chunk).expect("read");
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.contains(&b'\n') {
                break;
            }
        }
        std::thread::sleep(delay);
        stream.write_all(reply.as_bytes()).expect("write reply");
        stream.write_all(b"\n").expect("write nl");
        stream.flush().ok();
        let end = buf.iter().position(|&b| b == b'\n').unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..end]).to_string()
    })
}

/// A bound service socket nothing answers on, to prove a refused call never
/// reached the service.
#[cfg(unix)]
fn silent_stub(path: &std::path::Path) -> std::os::unix::net::UnixListener {
    let listener = std::os::unix::net::UnixListener::bind(path).expect("bind stub socket");
    listener.set_nonblocking(true).unwrap();
    listener
}

#[cfg(unix)]
fn assert_never_contacted(listener: &std::os::unix::net::UnixListener) {
    let err = listener
        .accept()
        .expect_err("the service must not be contacted");
    assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock);
}

#[cfg(unix)]
#[tokio::test]
async fn gpio_output_set_forwards_a_set_request_and_returns_the_reply() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gpio-cmd.sock");
    let stub = gpio_stub(
        path.clone(),
        r#"{"ok":true,"chip":0,"pin":17,"level":"high"}"#,
    );
    let host = RealHost::new().with_gpio_cmd_path(path);

    let args = map(&[("pin", Value::from(17)), ("level", Value::from("high"))]);
    let m = ok_map(host.gpio_output_set("p", &args).await);
    // The reply round-trips back to the plugin verbatim.
    assert_eq!(field(&m, "ok").and_then(Value::as_bool), Some(true));
    assert_eq!(field(&m, "pin").and_then(Value::as_i64), Some(17));
    assert_eq!(field(&m, "level").and_then(Value::as_str), Some("high"));

    // The forwarded request carried the op + the validated fields.
    let sent = stub.join().unwrap();
    let v: serde_json::Value = serde_json::from_str(&sent).unwrap();
    assert_eq!(v["op"], "set");
    assert_eq!(v["pin"], 17);
    assert_eq!(v["level"], "high");
    assert_eq!(v["chip"], 0);
}

#[cfg(unix)]
#[tokio::test]
async fn gpio_buzzer_beep_forwards_the_envelope() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gpio-cmd.sock");
    let stub = gpio_stub(path.clone(), r#"{"ok":true,"phases":4}"#);
    let host = RealHost::new().with_gpio_cmd_path(path);

    let args = map(&[
        ("pin", Value::from(18)),
        ("on_ms", Value::from(120)),
        ("off_ms", Value::from(80)),
        ("cycles", Value::from(2)),
        ("freq_hz", Value::from(2700)),
    ]);
    let m = ok_map(host.gpio_buzzer_beep("p", &args).await);
    assert_eq!(field(&m, "ok").and_then(Value::as_bool), Some(true));
    assert_eq!(field(&m, "phases").and_then(Value::as_i64), Some(4));

    let sent = stub.join().unwrap();
    let v: serde_json::Value = serde_json::from_str(&sent).unwrap();
    assert_eq!(v["op"], "beep");
    assert_eq!(v["pin"], 18);
    assert_eq!(v["on_ms"], 120);
    assert_eq!(v["off_ms"], 80);
    assert_eq!(v["cycles"], 2);
    assert_eq!(v["freq_hz"], 2700);
}

#[tokio::test]
async fn gpio_set_validates_args_before_forwarding() {
    // A bad request fails validation in the host with no socket touched.
    let host = RealHost::new();
    assert_eq!(
        err_body(
            host.gpio_output_set("p", &map(&[("level", Value::from("high"))]))
                .await
        ),
        "pin must be an integer"
    );
    assert_eq!(
        err_body(
            host.gpio_output_set("p", &map(&[("pin", Value::from(17))]))
                .await
        ),
        "level must be \"high\" or \"low\""
    );
    assert_eq!(
        err_body(
            host.gpio_output_set(
                "p",
                &map(&[("pin", Value::from(17)), ("level", Value::from("mid"))])
            )
            .await
        ),
        "level must be \"high\" or \"low\""
    );
    assert_eq!(
        err_body(
            host.gpio_buzzer_beep(
                "p",
                &map(&[("pin", Value::from(18)), ("cycles", Value::from(2))])
            )
            .await
        ),
        "on_ms must be an integer"
    );
}

#[tokio::test]
async fn gpio_set_degrades_to_not_available_when_the_service_is_absent() {
    // No socket bound: the forward reports not_available, never errors.
    let host = RealHost::new()
        .with_gpio_cmd_path(std::path::PathBuf::from("/nonexistent/ados-gpio-test.sock"));
    let args = map(&[("pin", Value::from(17)), ("level", Value::from("high"))]);
    let m = ok_map(host.gpio_output_set("p", &args).await);
    assert_eq!(
        field(&m, "error").and_then(Value::as_str),
        Some("not_available")
    );
    assert_eq!(
        field(&m, "method").and_then(Value::as_str),
        Some("gpio.output.set")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_gpio_service_that_never_answers_reads_as_unavailable() {
    // A wedged service accepts and never replies; the forward gives up on its
    // budget instead of holding the plugin's request open.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gpio-cmd.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let host = RealHost::new().with_gpio_cmd_path(path);
    let args = map(&[("pin", Value::from(17)), ("level", Value::from("high"))]);
    let m = ok_map(host.gpio_output_set("p", &args).await);
    assert_eq!(
        field(&m, "error").and_then(Value::as_str),
        Some("not_available")
    );
    // A wedged service is told apart from an absent one.
    assert_eq!(
        field(&m, "reason").and_then(Value::as_str),
        Some("gpio service did not answer within 2000 ms")
    );
}

// ---- video.source.set ------------------------------------------------

#[cfg(unix)]
#[tokio::test]
async fn video_source_set_forwards_the_source_list_and_returns_the_reply() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("video-cmd.sock");
    let stub = gpio_stub(path.clone(), r#"{"ok":true,"count":2}"#);
    let host = RealHost::new().with_video_cmd_path(path);

    let cameras = Value::Array(vec![
        map(&[
            ("id", Value::from("main")),
            ("source", Value::from("rtsp://192.168.144.25:8554/main")),
            ("role", Value::from("eo")),
        ]),
        map(&[
            ("id", Value::from("ir")),
            ("source", Value::from("rtsp://192.168.144.25:8554/ir")),
            ("role", Value::from("ir")),
        ]),
    ]);
    let args = map(&[("cameras", cameras)]);
    let m = ok_map(host.video_source_set("p", &args).await);
    // The reply round-trips back to the plugin verbatim.
    assert_eq!(field(&m, "ok").and_then(Value::as_bool), Some(true));
    assert_eq!(field(&m, "count").and_then(Value::as_i64), Some(2));

    // The forwarded request carried the op + the full leg list.
    let sent = stub.join().unwrap();
    let v: serde_json::Value = serde_json::from_str(&sent).unwrap();
    assert_eq!(v["op"], "video.source.set");
    assert_eq!(v["cameras"][0]["id"], "main");
    assert_eq!(v["cameras"][0]["source"], "rtsp://192.168.144.25:8554/main");
    assert_eq!(v["cameras"][1]["role"], "ir");
    // The request is attributed to the calling plugin so the supervisor's
    // merge-by-owner persist preserves the operator's legs.
    assert_eq!(v["owner"], "p");
}

#[cfg(unix)]
#[tokio::test]
async fn video_source_set_waits_out_the_pipeline_restart() {
    // The supervisor answers only after it has restarted the video service,
    // which takes seconds. A reply that late is the change being applied, not
    // an unavailable service.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("video-cmd.sock");
    let stub = gpio_stub_after(
        path.clone(),
        r#"{"ok":true,"count":1}"#,
        std::time::Duration::from_millis(2500),
    );
    let host = RealHost::new().with_video_cmd_path(path);
    let cameras = Value::Array(vec![map(&[
        ("id", Value::from("main")),
        ("source", Value::from("rtsp://192.168.1.50:8554/main")),
    ])]);
    let m = ok_map(
        host.video_source_set("p", &map(&[("cameras", cameras)]))
            .await,
    );
    assert_eq!(field(&m, "ok").and_then(Value::as_bool), Some(true));
    assert_eq!(field(&m, "error"), None);
    stub.join().unwrap();
}

#[tokio::test]
async fn video_source_set_rejects_a_non_array_or_empty_or_incomplete_list() {
    let host = RealHost::new();
    // Not an array.
    assert_eq!(
        err_body(
            host.video_source_set("p", &map(&[("cameras", Value::from("main"))]))
                .await
        ),
        "cameras must be an array"
    );
    // Empty array.
    assert_eq!(
        err_body(
            host.video_source_set("p", &map(&[("cameras", Value::Array(vec![]))]))
                .await
        ),
        "cameras must not be empty"
    );
    // A leg with no source cannot be served.
    let bad = Value::Array(vec![map(&[("id", Value::from("main"))])]);
    assert_eq!(
        err_body(host.video_source_set("p", &map(&[("cameras", bad)])).await),
        "each camera needs a non-empty id and source"
    );
}

#[tokio::test]
async fn video_source_set_degrades_to_not_available_when_the_service_is_absent() {
    // No socket bound: the forward reports not_available, never errors.
    let host = RealHost::new().with_video_cmd_path(std::path::PathBuf::from(
        "/nonexistent/ados-video-test.sock",
    ));
    let cameras = Value::Array(vec![map(&[
        ("id", Value::from("main")),
        ("source", Value::from("rtsp://x/main")),
    ])]);
    let m = ok_map(
        host.video_source_set("p", &map(&[("cameras", cameras)]))
            .await,
    );
    assert_eq!(
        field(&m, "error").and_then(Value::as_str),
        Some("not_available")
    );
    assert_eq!(
        field(&m, "method").and_then(Value::as_str),
        Some("video.source.set")
    );
}

#[test]
fn video_source_set_is_gated_on_the_video_source_capability() {
    use crate::dispatch::{gate, Gate, Method};
    assert_eq!(
        gate("video.source.set", false, &caps(&[])),
        Gate::CapabilityDenied("capability_denied: video.source.set".to_string())
    );
    assert_eq!(
        gate("video.source.set", false, &caps(&["video.source.set"])),
        Gate::Allow(Method::VideoSourceSet)
    );
}

#[test]
fn gpio_methods_are_gated_on_the_gpio_output_capability() {
    use crate::dispatch::{gate, Gate, Method};
    for (method, variant) in [
        ("gpio.output.set", Method::GpioOutputSet),
        ("gpio.buzzer.beep", Method::GpioBuzzerBeep),
    ] {
        assert_eq!(
            gate(method, false, &caps(&[])),
            Gate::CapabilityDenied("capability_denied: hardware.gpio_out".to_string())
        );
        assert_eq!(
            gate(method, false, &caps(&["hardware.gpio_out"])),
            Gate::Allow(variant)
        );
    }
}

// ---- radio.aux_stream.open / radio.aux_stream.close ------------------

#[cfg(unix)]
#[tokio::test]
async fn radio_aux_open_forwards_a_bare_open_and_records_the_owner() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("radio-aux.sock");
    let stub = gpio_stub(
        path.clone(),
        r#"{"ok":true,"active":true,"tx_port":5602,"rx_port":5603}"#,
    );
    let host = RealHost::new().with_radio_aux_cmd_path(path);

    let m = ok_map(host.radio_aux_stream_open("p", &map(&[])).await);
    // The reply round-trips back to the plugin verbatim.
    assert_eq!(field(&m, "ok").and_then(Value::as_bool), Some(true));
    assert_eq!(field(&m, "active").and_then(Value::as_bool), Some(true));
    assert_eq!(field(&m, "tx_port").and_then(Value::as_i64), Some(5602));

    // The forwarded request is a bare open — the plugin cannot pick a port.
    let sent = stub.join().unwrap();
    let v: serde_json::Value = serde_json::from_str(&sent).unwrap();
    assert_eq!(v["op"], "open");
    assert_eq!(v.as_object().unwrap().len(), 1);

    // Ownership recorded so a later disconnect closes the stream.
    assert_eq!(
        *host.aux_stream_owner.lock().unwrap(),
        Some("p".to_string())
    );
}

#[cfg(unix)]
#[tokio::test]
async fn radio_aux_open_while_another_plugin_owns_the_stream_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("radio-aux.sock");
    let service = silent_stub(&path);
    let host = RealHost::new().with_radio_aux_cmd_path(path);
    *host.aux_stream_owner.lock().unwrap() = Some("owner".to_string());

    assert_eq!(
        err_body(host.radio_aux_stream_open("intruder", &map(&[])).await),
        "radio aux stream is open by another plugin"
    );
    assert_never_contacted(&service);
    // The owner keeps the stream: its disconnect still closes the pair.
    assert_eq!(
        *host.aux_stream_owner.lock().unwrap(),
        Some("owner".to_string())
    );
}

#[cfg(unix)]
#[tokio::test]
async fn radio_aux_send_forwards_the_aux_framed_datagram() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("radio-aux.sock");
    let stub = gpio_stub(path.clone(), r#"{"ok":true}"#);
    let host = RealHost::new().with_radio_aux_cmd_path(path);
    *host.aux_stream_owner.lock().unwrap() = Some("p".to_string());

    // A payload on the AppStream channel (8). The host frames it as an aux
    // datagram, so the forwarded `frame` is the encoded bytes and the radio
    // service writes them verbatim to the aux transmit ingress.
    let payload: Vec<u8> = b"hello".to_vec();
    let args = map(&[
        ("channel", Value::from(8)),
        ("payload", Value::Binary(payload)),
    ]);
    let m = ok_map(host.radio_aux_stream_send("p", &args).await);
    assert_eq!(field(&m, "ok").and_then(Value::as_bool), Some(true));

    let sent = stub.join().unwrap();
    let v: serde_json::Value = serde_json::from_str(&sent).unwrap();
    assert_eq!(v["op"], "send");
    // The encoded AppStream frame for "hello":
    // magic [0xAD,0x02] + version 0x01 + channel 0x08 + len 0x0005 + payload.
    assert_eq!(
        v["frame"],
        serde_json::json!([0xAD, 0x02, 0x01, 0x08, 0x00, 0x05, 104, 101, 108, 108, 111])
    );
    // A send never touches the owner record (the stream stays between the
    // matching open and close).
    assert_eq!(
        *host.aux_stream_owner.lock().unwrap(),
        Some("p".to_string())
    );
}

#[cfg(unix)]
#[tokio::test]
async fn radio_aux_send_accepts_both_application_channels() {
    // Two sends against two one-shot stubs: AppStream (8) and AppCommand (9)
    // both forward.
    for (i, channel) in [8u64, 9u64].iter().enumerate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("radio-aux-{i}.sock"));
        let stub = gpio_stub(path.clone(), r#"{"ok":true}"#);
        let host = RealHost::new().with_radio_aux_cmd_path(path);
        *host.aux_stream_owner.lock().unwrap() = Some("p".to_string());
        let args = map(&[
            ("channel", Value::from(*channel)),
            ("payload", Value::Binary(vec![1, 2, 3])),
        ]);
        let m = ok_map(host.radio_aux_stream_send("p", &args).await);
        assert_eq!(field(&m, "ok").and_then(Value::as_bool), Some(true));
        let sent = stub.join().unwrap();
        let v: serde_json::Value = serde_json::from_str(&sent).unwrap();
        assert_eq!(
            v["frame"][3], *channel,
            "channel byte must be echoed into the frame"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn radio_aux_send_from_a_plugin_that_does_not_own_the_stream_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("radio-aux.sock");
    let service = silent_stub(&path);
    let host = RealHost::new().with_radio_aux_cmd_path(path);
    let args = map(&[
        ("channel", Value::from(8)),
        ("payload", Value::Binary(vec![1, 2, 3])),
    ]);
    // Nobody has opened it through the host, and then another plugin has.
    for owner in [None, Some("owner".to_string())] {
        *host.aux_stream_owner.lock().unwrap() = owner;
        assert_eq!(
            err_body(host.radio_aux_stream_send("intruder", &args).await),
            "radio aux stream is not open by this plugin"
        );
    }
    assert_never_contacted(&service);
}

#[tokio::test]
async fn radio_aux_send_rejects_bad_channel_or_payload_before_forwarding() {
    // Validation happens before any socket IO, so no socket is needed.
    let host = RealHost::new();
    // Missing channel.
    assert_eq!(
        err_body(
            host.radio_aux_stream_send("p", &map(&[("payload", Value::Binary(vec![1]))]))
                .await
        ),
        "channel missing or not an integer"
    );
    // Unsupported channel (this host is profile-agnostic; only 8/9 apply).
    assert_eq!(
        err_body(
            host.radio_aux_stream_send(
                "p",
                &map(&[
                    ("channel", Value::from(3)),
                    ("payload", Value::Binary(vec![1])),
                ])
            )
            .await
        ),
        "unsupported aux channel 3"
    );
    // A channel past the byte range is refused, not wrapped onto 8 or 9.
    assert_eq!(
        err_body(
            host.radio_aux_stream_send(
                "p",
                &map(&[
                    ("channel", Value::from(264)),
                    ("payload", Value::Binary(vec![1])),
                ])
            )
            .await
        ),
        "unsupported aux channel 264"
    );
    // Missing payload.
    assert_eq!(
        err_body(
            host.radio_aux_stream_send("p", &map(&[("channel", Value::from(8))]))
                .await
        ),
        "payload missing"
    );
    // A string payload is not bytes.
    assert_eq!(
        err_body(
            host.radio_aux_stream_send(
                "p",
                &map(&[
                    ("channel", Value::from(8)),
                    ("payload", Value::from("hello")),
                ])
            )
            .await
        ),
        "payload must be bytes"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn radio_aux_close_forwards_and_clears_the_owner() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("radio-aux.sock");
    let host = RealHost::new().with_radio_aux_cmd_path(path.clone());
    // Seed ownership as if this plugin had opened the stream.
    *host.aux_stream_owner.lock().unwrap() = Some("p".to_string());

    let stub = gpio_stub(path, r#"{"ok":true,"active":false}"#);
    let m = ok_map(host.radio_aux_stream_close("p", &map(&[])).await);
    assert_eq!(field(&m, "active").and_then(Value::as_bool), Some(false));

    let sent = stub.join().unwrap();
    let v: serde_json::Value = serde_json::from_str(&sent).unwrap();
    assert_eq!(v["op"], "close");

    // The owner record is cleared on a confirmed close.
    assert!(host.aux_stream_owner.lock().unwrap().is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn radio_aux_close_by_a_non_owner_is_refused_and_never_reaches_the_service() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("radio-aux.sock");
    let service = silent_stub(&path);
    let host = RealHost::new().with_radio_aux_cmd_path(path);
    // Another plugin owns the stream.
    *host.aux_stream_owner.lock().unwrap() = Some("owner".to_string());

    assert_eq!(
        err_body(host.radio_aux_stream_close("intruder", &map(&[])).await),
        "radio aux stream is not open by this plugin"
    );
    // The owner's link was never torn down, and its record survives.
    assert_never_contacted(&service);
    assert_eq!(
        *host.aux_stream_owner.lock().unwrap(),
        Some("owner".to_string())
    );
}

#[cfg(unix)]
#[tokio::test]
async fn release_plugin_closes_an_aux_stream_the_plugin_owned() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("radio-aux.sock");
    let host = RealHost::new().with_radio_aux_cmd_path(path.clone());
    *host.aux_stream_owner.lock().unwrap() = Some("p".to_string());

    let stub = gpio_stub(path, r#"{"ok":true,"active":false}"#);
    host.release_session("p", 0).await;
    // The disconnect forwarded a close (safe-by-default: the stream never
    // outlives its owner).
    let sent = stub.join().unwrap();
    let v: serde_json::Value = serde_json::from_str(&sent).unwrap();
    assert_eq!(v["op"], "close");
    assert!(host.aux_stream_owner.lock().unwrap().is_none());
}

#[tokio::test]
async fn radio_aux_open_degrades_to_not_available_when_the_service_is_absent() {
    // No socket bound: the forward reports not_available, never errors, and
    // NO ownership is recorded (so a later disconnect forwards nothing).
    let host = RealHost::new()
        .with_radio_aux_cmd_path(std::path::PathBuf::from("/nonexistent/ados-aux-test.sock"));
    let m = ok_map(host.radio_aux_stream_open("p", &map(&[])).await);
    assert_eq!(
        field(&m, "error").and_then(Value::as_str),
        Some("not_available")
    );
    assert_eq!(
        field(&m, "method").and_then(Value::as_str),
        Some("radio.aux_stream.open")
    );
    // A failed open never claims ownership.
    assert!(host.aux_stream_owner.lock().unwrap().is_none());
}

#[test]
fn radio_aux_methods_are_gated_on_the_aux_stream_capability() {
    use crate::dispatch::{gate, Gate, Method};
    for (method, variant) in [
        ("radio.aux_stream.open", Method::RadioAuxStreamOpen),
        ("radio.aux_stream.close", Method::RadioAuxStreamClose),
        ("radio.aux_stream.send", Method::RadioAuxStreamSend),
        (
            "radio.aux_stream.subscribe",
            Method::RadioAuxStreamSubscribe,
        ),
    ] {
        assert_eq!(
            gate(method, false, &caps(&[])),
            Gate::CapabilityDenied("capability_denied: radio.aux_stream".to_string())
        );
        assert_eq!(
            gate(method, false, &caps(&["radio.aux_stream"])),
            Gate::Allow(variant)
        );
    }
}

// ---- guided setpoint sender -----------------------------------------

/// A pure-velocity local-NED setpoint request: ignore position / accel / yaw,
/// command vx/vy/vz, body frame (8). Mirrors the builder's velocity_setpoint.
fn velocity_args() -> Value {
    // type_mask: X|Y|Z | AX|AY|AZ | YAW | YAW_RATE = 1+2+4 +64+128+256 +1024+2048.
    let mask = 1 + 2 + 4 + 64 + 128 + 256 + 1024 + 2048;
    map(&[
        ("kind", Value::from("local_ned")),
        ("coordinate_frame", Value::from(8)), // MAV_FRAME_BODY_NED
        ("type_mask", Value::from(mask)),
        ("vx", Value::F64(2.5)),
        ("vy", Value::F64(-1.0)),
        ("vz", Value::F64(0.5)),
    ])
}

#[test]
fn guided_setpoint_is_gated_on_the_capability() {
    use crate::dispatch::{gate, Gate, Method};
    assert_eq!(
        gate("flight.guided_setpoint.send", false, &caps(&[])),
        Gate::CapabilityDenied("capability_denied: flight.guided_setpoint".to_string())
    );
    assert_eq!(
        gate(
            "flight.guided_setpoint.send",
            false,
            &caps(&["flight.guided_setpoint"])
        ),
        Gate::Allow(Method::GuidedSetpointSend)
    );
}

#[test]
fn guided_setpoint_without_router_degrades_to_not_available() {
    // No mavlink client wired: degrade, never error (the mavlink.send posture).
    let host = host_with_fc();
    let m = ok_map(host.guided_setpoint_send("p", &velocity_args()));
    assert_eq!(
        field(&m, "error").and_then(Value::as_str),
        Some("not_available")
    );
    assert_eq!(
        field(&m, "method").and_then(Value::as_str),
        Some("flight.guided_setpoint.send")
    );
}

#[test]
fn guided_setpoint_validates_args_before_any_send() {
    let host = host_with_fc();
    // Missing kind.
    assert_eq!(
        err_body(host.guided_setpoint_send("p", &map(&[("type_mask", Value::from(0))]))),
        "kind must be \"local_ned\" or \"global_int\""
    );
    // Missing coordinate_frame.
    assert_eq!(
        err_body(host.guided_setpoint_send(
            "p",
            &map(&[
                ("kind", Value::from("local_ned")),
                ("type_mask", Value::from(0))
            ]),
        )),
        "coordinate_frame must be an integer"
    );
    // A NaN on an active axis (vx is active under this mask) is rejected by
    // the builder's validation.
    let mut bad = match velocity_args() {
        Value::Map(mut m) => {
            m.retain(|(k, _)| k.as_str() != Some("vx"));
            m.push((Value::from("vx"), Value::F64(f64::NAN)));
            Value::Map(m)
        }
        other => panic!("{other:?}"),
    };
    assert_eq!(
        err_body(host.guided_setpoint_send("p", &bad)),
        "vx must be a finite number"
    );
    // A frame wrong for the kind (a global frame on a local message).
    bad = map(&[
        ("kind", Value::from("local_ned")),
        ("coordinate_frame", Value::from(5)), // MAV_FRAME_GLOBAL_INT
        ("type_mask", Value::from(0)),
    ]);
    assert!(
        err_body(host.guided_setpoint_send("p", &bad)).contains("not valid for this setpoint kind")
    );
    // A type_mask with an unknown high bit.
    bad = map(&[
        ("kind", Value::from("local_ned")),
        ("coordinate_frame", Value::from(8)),
        ("type_mask", Value::from(0x8000)),
    ]);
    assert!(err_body(host.guided_setpoint_send("p", &bad))
        .contains("outside the defined position-target field"));
}

#[tokio::test]
// Asserts on the spec-superseded MAV_FRAME_BODY_NED, which is what the
// guided-setpoint builder emits and ArduPilot reads; see
// `ados_protocol::mavlink::mav_frame_from_u8`.
#[allow(deprecated)]
async fn guided_setpoint_frame_reaches_the_router_and_decodes() {
    // With a live mavlink client wired to a stub router socket, the handler
    // builds the SET_POSITION_TARGET_LOCAL_NED (84) frame and writes it; the
    // router side reads it back and it decodes to the same message + fields.
    use crate::frame_link::{FrameLink, LINK_DEPTH};
    use ados_protocol::ipc::IpcBroadcast;
    use ados_protocol::mavlink::{ardupilotmega, parse_v2, MavMessage};

    let mut sock = std::env::temp_dir();
    sock.push(format!("ados-realhost-sp-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);
    let (_server, inbound) = IpcBroadcast::bind(&sock, LINK_DEPTH, false, Some(16))
        .await
        .unwrap();
    let mut inbound = inbound.expect("inbound channel requested");

    let client = std::sync::Arc::new(FrameLink::spawn(&sock, Box::new(Vec::new)));
    assert!(
        client
            .connected_within(std::time::Duration::from_secs(2))
            .await
    );
    // A fleet drone with system id 3: the setpoint must be addressed to it.
    let host = RealHost::new().with_mavlink(client);
    host.fc_identity().set(3, 1);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let m = ok_map(host.guided_setpoint_send("p", &velocity_args()));
    assert_eq!(field(&m, "sent").and_then(Value::as_bool), Some(true));
    assert_eq!(field(&m, "msg_id").and_then(Value::as_i64), Some(84));

    // The router reads the raw frame; decode it and check the fields.
    let frame = tokio::time::timeout(std::time::Duration::from_millis(500), inbound.recv())
        .await
        .unwrap()
        .expect("a frame arrives")
        .payload;
    let (_h, decoded) = parse_v2(&frame).expect("decode succeeds");
    match decoded {
        MavMessage::SET_POSITION_TARGET_LOCAL_NED(d) => {
            assert_eq!(d.vx, 2.5);
            assert_eq!(d.vy, -1.0);
            assert_eq!(d.vz, 0.5);
            assert_eq!(d.target_system, 3);
            assert_eq!(d.target_component, 1);
            assert_eq!(
                d.coordinate_frame,
                ardupilotmega::MavFrame::MAV_FRAME_BODY_NED
            );
        }
        other => panic!("expected SET_POSITION_TARGET_LOCAL_NED, got {other:?}"),
    }
}

#[tokio::test]
async fn a_command_the_pic_gate_refuses_reports_pic_refused_and_is_never_queued() {
    // With injector arbitration armed, the router drops the host's commands
    // while an operator holds PIC and says nothing back. The plugin must read
    // that refusal, not `sent: true`, and the frame must not be queued.
    use crate::frame_link::{FrameLink, LINK_DEPTH};
    use crate::pic_gate::PicGate;
    use ados_protocol::ipc::IpcBroadcast;

    let dir = tempfile::tempdir().unwrap();
    let sidecar = dir.path().join("pic-state.json");
    std::fs::write(&sidecar, r#"{"state":"claimed","claimed_by":"hdmi-kiosk"}"#).unwrap();

    let mut sock = std::env::temp_dir();
    sock.push(format!("ados-realhost-pic-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);
    let (_server, inbound) = IpcBroadcast::bind(&sock, LINK_DEPTH, false, Some(16))
        .await
        .unwrap();
    let mut inbound = inbound.expect("inbound channel requested");

    let link = FrameLink::spawn(&sock, Box::new(Vec::new))
        .with_pic_gate(PicGate::with_path(sidecar.clone(), "plugin-host"));
    let client = std::sync::Arc::new(link);
    assert!(
        client
            .connected_within(std::time::Duration::from_secs(2))
            .await
    );
    let host = RealHost::new().with_mavlink(client);
    host.fc_identity().set(3, 1);

    let m = ok_map(host.guided_setpoint_send("p", &velocity_args()));
    assert_eq!(field(&m, "sent").and_then(Value::as_bool), Some(false));
    assert_eq!(
        field(&m, "reason").and_then(Value::as_str),
        Some("pic_refused")
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), inbound.recv())
            .await
            .is_err(),
        "a refused command never reaches the router"
    );

    // The operator releases PIC: the next command goes out.
    std::fs::write(&sidecar, r#"{"state":"unclaimed"}"#).unwrap();
    let m = ok_map(host.guided_setpoint_send("p", &velocity_args()));
    assert_eq!(field(&m, "sent").and_then(Value::as_bool), Some(true));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(500), inbound.recv())
            .await
            .unwrap()
            .is_some()
    );
}

#[test]
fn command_target_comes_from_args_then_the_observed_autopilot() {
    let none = map(&[]);
    // Nothing observed and nothing passed: refused, never a guessed 1/1.
    assert!(command_target(&none, None).is_err());
    assert_eq!(command_target(&none, Some((3, 1))).unwrap(), (3, 1));
    let explicit = map(&[
        ("target_system", Value::from(7)),
        ("target_component", Value::from(2)),
    ]);
    assert_eq!(command_target(&explicit, None).unwrap(), (7, 2));
    assert_eq!(command_target(&explicit, Some((3, 1))).unwrap(), (7, 2));
    let sys_only = map(&[("target_system", Value::from(9))]);
    assert_eq!(command_target(&sys_only, Some((3, 1))).unwrap(), (9, 1));
    let bad = map(&[("target_system", Value::from(300))]);
    assert!(command_target(&bad, Some((3, 1))).is_err());
}

#[tokio::test]
async fn guided_setpoint_global_int_builds_and_decodes() {
    // A global-int setpoint with a commanded position decodes back to msg 86
    // with the scaled lat/lon and altitude intact.
    use crate::frame_link::{FrameLink, LINK_DEPTH};
    use ados_protocol::ipc::IpcBroadcast;
    use ados_protocol::mavlink::{parse_v2, MavMessage};

    let mut sock = std::env::temp_dir();
    sock.push(format!("ados-realhost-sp-gi-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);
    let (_server, inbound) = IpcBroadcast::bind(&sock, LINK_DEPTH, false, Some(16))
        .await
        .unwrap();
    let mut inbound = inbound.expect("inbound channel requested");

    let client = std::sync::Arc::new(FrameLink::spawn(&sock, Box::new(Vec::new)));
    assert!(
        client
            .connected_within(std::time::Duration::from_secs(2))
            .await
    );
    let host = host_with_fc().with_mavlink(client);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Position + velocity: clear the X/Y/Z ignore bits, keep accel/yaw ignored.
    let mask = 64 + 128 + 256 + 1024 + 2048;
    let args = map(&[
        ("kind", Value::from("global_int")),
        ("coordinate_frame", Value::from(6)), // GLOBAL_RELATIVE_ALT_INT
        ("type_mask", Value::from(mask)),
        ("x", Value::F64(374_224_080.0)), // scaled latitude (37.422408 * 1e7)
        ("y", Value::F64(-1_220_842_700.0)),
        ("z", Value::F64(30.0)),
        ("vz", Value::F64(-0.5)),
        ("target_system", Value::from(2)),
    ]);
    let m = ok_map(host.guided_setpoint_send("p", &args));
    assert_eq!(field(&m, "msg_id").and_then(Value::as_i64), Some(86));

    let frame = tokio::time::timeout(std::time::Duration::from_millis(500), inbound.recv())
        .await
        .unwrap()
        .expect("a frame arrives")
        .payload;
    match parse_v2(&frame).expect("decode succeeds").1 {
        MavMessage::SET_POSITION_TARGET_GLOBAL_INT(d) => {
            assert_eq!(d.lat_int, 374_224_080);
            assert_eq!(d.lon_int, -1_220_842_700);
            assert_eq!(d.alt, 30.0);
            assert_eq!(d.vz, -0.5);
            assert_eq!(d.target_system, 2, "target override is honoured");
        }
        other => panic!("expected SET_POSITION_TARGET_GLOBAL_INT, got {other:?}"),
    }
}

// ---- mavlink.tunnel.send --------------------------------------------

#[test]
fn mavlink_tunnel_send_is_gated_on_the_tunnel_capability() {
    use crate::dispatch::{gate, Gate, Method};
    // The plain mavlink.write does not satisfy the tunnel cap.
    assert_eq!(
        gate("mavlink.tunnel.send", false, &caps(&["mavlink.write"])),
        Gate::CapabilityDenied("capability_denied: mavlink.tunnel".to_string())
    );
    assert_eq!(
        gate("mavlink.tunnel.send", false, &caps(&["mavlink.tunnel"])),
        Gate::Allow(Method::MavlinkTunnelSend)
    );
}

#[test]
fn mavlink_tunnel_send_without_router_degrades_to_not_available() {
    // No mavlink client wired: degrade, never error (the mavlink.send posture).
    let host = host_with_fc();
    let args = map(&[
        ("payload_type", Value::from(40001)),
        ("payload", Value::Binary(vec![1, 2, 3])),
    ]);
    let m = ok_map(host.mavlink_tunnel_send("p", &args));
    assert_eq!(
        field(&m, "error").and_then(Value::as_str),
        Some("not_available")
    );
    assert_eq!(
        field(&m, "method").and_then(Value::as_str),
        Some("mavlink.tunnel.send")
    );
}

#[test]
fn mavlink_tunnel_send_validates_before_any_send() {
    let host = host_with_fc();
    // Missing payload_type.
    assert_eq!(
        err_body(host.mavlink_tunnel_send("p", &map(&[("payload", Value::Binary(vec![1]))]))),
        "payload_type is required"
    );
    // A registered (non-private) payload_type is refused by the builder.
    let registered = map(&[
        ("payload_type", Value::from(200)),
        ("payload", Value::Binary(vec![1])),
    ]);
    assert!(err_body(host.mavlink_tunnel_send("p", &registered)).contains("private type"));
    // A wrong payload type (a string) is rejected, not coerced.
    let bad_payload = map(&[
        ("payload_type", Value::from(40001)),
        ("payload", Value::from("not-bytes")),
    ]);
    assert_eq!(
        err_body(host.mavlink_tunnel_send("p", &bad_payload)),
        "payload must be bytes"
    );
    // An oversized payload is refused.
    let oversize = map(&[
        ("payload_type", Value::from(40001)),
        (
            "payload",
            Value::Binary(vec![0u8; ados_protocol::mavlink::TUNNEL_MAX_PAYLOAD + 1]),
        ),
    ]);
    assert!(err_body(host.mavlink_tunnel_send("p", &oversize)).contains("exceeds"));
}

#[tokio::test]
async fn mavlink_tunnel_send_frame_reaches_the_router_and_round_trips_the_payload() {
    // With a live mavlink client wired to a stub router socket, the handler
    // builds the TUNNEL (385) frame and writes it; the router side reads it
    // back, the classifier recovers the private payload_type off the wire,
    // and the application payload round-trips byte-for-byte.
    use crate::frame_link::{FrameLink, LINK_DEPTH};
    use ados_protocol::ipc::IpcBroadcast;
    use ados_protocol::mavlink::{tunnel_payload_type, MSG_ID_TUNNEL};

    let mut sock = std::env::temp_dir();
    sock.push(format!("ados-realhost-tun-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);
    let (_server, inbound) = IpcBroadcast::bind(&sock, LINK_DEPTH, false, Some(16))
        .await
        .unwrap();
    let mut inbound = inbound.expect("inbound channel requested");

    let client = std::sync::Arc::new(FrameLink::spawn(&sock, Box::new(Vec::new)));
    assert!(
        client
            .connected_within(std::time::Duration::from_secs(2))
            .await
    );
    let host = host_with_fc().with_mavlink(client);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let payload_type = 40001;
    let app_payload = b"opaque-app-bytes".to_vec();
    let args = map(&[
        ("payload_type", Value::from(payload_type)),
        ("payload", Value::Binary(app_payload.clone())),
        ("target_system", Value::from(2)),
    ]);
    let m = ok_map(host.mavlink_tunnel_send("p", &args));
    assert_eq!(field(&m, "sent").and_then(Value::as_bool), Some(true));
    assert_eq!(
        field(&m, "payload_type").and_then(Value::as_i64),
        Some(payload_type as i64)
    );
    assert_eq!(
        field(&m, "payload_len").and_then(Value::as_i64),
        Some(app_payload.len() as i64)
    );

    // The router reads the raw frame; it is a TUNNEL carrying the private
    // type, and the application payload bytes survive verbatim.
    let frame = tokio::time::timeout(std::time::Duration::from_millis(500), inbound.recv())
        .await
        .unwrap()
        .expect("a frame arrives")
        .payload;
    let mut id = [0u8; 4];
    id[..3].copy_from_slice(&frame[7..10]);
    assert_eq!(u32::from_le_bytes(id), MSG_ID_TUNNEL);
    assert_eq!(tunnel_payload_type(&frame), Some(payload_type));
    // TUNNEL wire layout after the 10-byte v2 header: payload_type (bytes
    // 10..12), target_system (12), target_component (13), payload_length
    // (14), then the payload at byte 15.
    assert_eq!(frame[12], 2, "target_system override rode through");
    assert_eq!(frame[14] as usize, app_payload.len());
    assert_eq!(&frame[15..15 + app_payload.len()], &app_payload[..]);

    let _ = std::fs::remove_file(&sock);
}

#[tokio::test]
async fn a_tap_already_on_disk_is_not_replayed_to_the_first_subscriber() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lcd-plugin-tap.json");
    std::fs::write(&path, r#"{"ts_ms":100,"key":"confirm"}"#).unwrap();
    let (tx, _keep) = broadcast::channel(8);
    let seen = read_tap_record_sync(&path);
    let mut rx = tx.subscribe();
    let task = tokio::spawn(display_tap_poll_loop(tx, path.clone(), seen));
    tokio::time::sleep(DISPLAY_TAP_POLL_INTERVAL * 3).await;
    assert!(
        rx.try_recv().is_err(),
        "the tap left over from before the watcher started is not a new tap"
    );
    std::fs::write(&path, r#"{"ts_ms":200,"key":"back"}"#).unwrap();
    let got = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("a new tap is delivered")
        .unwrap();
    assert_eq!(got, b"back");
    task.abort();
}

/// A stand-in cloud relay: accepts `count` connections on `path`, decodes each
/// request frame, and answers with `reply`. Returns the decoded requests.
fn cloud_relay_stub(
    path: std::path::PathBuf,
    count: usize,
    reply: ados_protocol::cloud_publish::CloudPublishReply,
) -> tokio::task::JoinHandle<Vec<ados_protocol::cloud_publish::CloudPublishRequest>> {
    let listener = tokio::net::UnixListener::bind(&path).expect("bind relay stub");
    tokio::spawn(async move {
        let mut seen = Vec::new();
        for _ in 0..count {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut len = [0u8; 4];
            stream.read_exact(&mut len).await.expect("read len");
            let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
            stream.read_exact(&mut body).await.expect("read body");
            seen.push(
                ados_protocol::cloud_publish::CloudPublishRequest::decode(&body).expect("decode"),
            );
            stream
                .write_all(&reply.encode().expect("encode reply"))
                .await
                .expect("write reply");
        }
        seen
    })
}

#[tokio::test]
async fn cloud_publish_forwards_under_the_callers_identity() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("cloud-publish.sock");
    let relay = cloud_relay_stub(
        sock.clone(),
        2,
        ados_protocol::cloud_publish::CloudPublishReply::accepted(),
    );
    let host = RealHost::new().with_cloud_publish_path(sock);

    let args = map(&[
        ("stream", Value::from("track.pose")),
        ("payload", Value::Binary(vec![1, 2, 3])),
        // A plugin cannot name itself someone else.
        ("plugin_id", Value::from("com.example.other")),
    ]);
    let m = ok_map(
        host.cloud_publish("com.example.mapper", &args, &caps(&["cloud.publish"]))
            .await,
    );
    assert_eq!(field(&m, "ok").and_then(Value::as_bool), Some(true));

    let record = map(&[
        ("collection", Value::from("jobs")),
        ("key", Value::from("job-1")),
        (
            "data",
            Value::Map(vec![(Value::from("state"), Value::from("done"))]),
        ),
        ("device_id", Value::from("drone01")),
    ]);
    let m = ok_map(host.cloud_records_put("com.example.mapper", &record).await);
    assert_eq!(field(&m, "ok").and_then(Value::as_bool), Some(true));

    let seen = relay.await.unwrap();
    assert_eq!(seen[0].plugin_id, "com.example.mapper");
    assert_eq!(seen[0].stream.as_deref(), Some("track.pose"));
    assert_eq!(seen[0].payload, vec![1, 2, 3]);
    assert_eq!(seen[1].plugin_id, "com.example.mapper");
    assert_eq!(seen[1].collection.as_deref(), Some("jobs"));
    assert_eq!(seen[1].key.as_deref(), Some("job-1"));
    assert_eq!(seen[1].device_id.as_deref(), Some("drone01"));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&seen[1].payload).unwrap(),
        serde_json::json!({"state": "done"})
    );
}

#[tokio::test]
async fn the_shared_detection_stream_needs_the_detection_capability() {
    let dir = tempfile::tempdir().unwrap();
    let host = RealHost::new().with_cloud_publish_path(dir.path().join("absent.sock"));
    let args = map(&[
        ("stream", Value::from("vision.detections")),
        ("payload", Value::Binary(b"{}".to_vec())),
    ]);
    assert_eq!(
        err_body(
            host.cloud_publish("com.example.detector", &args, &caps(&["cloud.publish"]))
                .await
        ),
        "capability_denied: vision.detection.publish"
    );
    // With it, the request goes out; the relay being down is the
    // not_available shape, not a refusal.
    let m = ok_map(
        host.cloud_publish(
            "com.example.detector",
            &args,
            &caps(&["cloud.publish", "vision.detection.publish"]),
        )
        .await,
    );
    assert_eq!(
        field(&m, "error").and_then(Value::as_str),
        Some("not_available")
    );
}

#[tokio::test]
async fn a_relay_refusal_and_a_bad_name_are_errors_to_the_plugin() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("cloud-publish.sock");
    let relay = cloud_relay_stub(
        sock.clone(),
        1,
        ados_protocol::cloud_publish::CloudPublishReply::refused("record too large"),
    );
    let host = RealHost::new().with_cloud_publish_path(sock);
    let bad = map(&[
        ("stream", Value::from("Not A Stream")),
        ("payload", Value::Binary(vec![])),
    ]);
    assert!(host.cloud_publish("p", &bad, &caps(&[])).await.is_err());
    let record = map(&[
        ("collection", Value::from("jobs")),
        ("key", Value::from("k")),
        ("data", Value::from(1)),
    ]);
    assert_eq!(
        err_body(host.cloud_records_put("com.example.mapper", &record).await),
        "cloud relay refused: record too large"
    );
    relay.await.unwrap();
}

#[test]
fn cloud_methods_are_gated_on_their_capabilities() {
    use crate::dispatch::{gate, Gate, Method};
    for (method, variant, cap) in [
        ("cloud.publish", Method::CloudPublish, "cloud.publish"),
        (
            "cloud.records.put",
            Method::CloudRecordsPut,
            "cloud.records",
        ),
    ] {
        assert_eq!(
            gate(method, false, &caps(&[])),
            Gate::CapabilityDenied(format!("capability_denied: {cap}"))
        );
        assert_eq!(gate(method, false, &caps(&[cap])), Gate::Allow(variant));
    }
}

#[tokio::test]
async fn an_advertised_offload_link_is_what_the_tier_readers_parse() {
    use ados_protocol::offload_link::read_offload_link_from;
    let dir = tempfile::tempdir().unwrap();
    let sidecar = dir.path().join("offload-link.json");
    let host = RealHost::new().with_offload_link_path(sidecar.clone());
    let args = map(&[
        ("paired", Value::Boolean(true)),
        ("bearer_acceptable", Value::Boolean(true)),
        ("target", Value::from("node.local:8092")),
        ("device_id", Value::from("ws-1")),
        ("model_id", Value::from("yolo")),
    ]);
    let m = ok_map(host.offload_advertise("com.example.offload", &args).await);
    assert_eq!(field(&m, "ok").and_then(Value::as_bool), Some(true));

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let link = read_offload_link_from(&sidecar, now).expect("fresh link parses");
    assert!(link.is_offload_path());
    assert_eq!(link.target.as_deref(), Some("node.local:8092"));
    assert_eq!(link.device_id.as_deref(), Some("ws-1"));
    assert_eq!(link.model_id.as_deref(), Some("yolo"));
    assert_eq!(
        link.version,
        ados_protocol::offload_link::OFFLOAD_LINK_SIDECAR_VERSION
    );
    // The host stamps the write time, so the link ages out on its own.
    let stale = now + ados_protocol::offload_link::OFFLOAD_LINK_STALE_MS + 1_000;
    assert!(read_offload_link_from(&sidecar, stale).is_none());

    // An honest "no link": paired false, nulls allowed.
    let none = map(&[
        ("paired", Value::Boolean(false)),
        ("bearer_acceptable", Value::Boolean(false)),
        ("target", Value::Nil),
    ]);
    ok_map(host.offload_advertise("com.example.offload", &none).await);
    let link = read_offload_link_from(&sidecar, now).unwrap();
    assert!(!link.is_offload_path());
    assert_eq!(link.target, None);

    // Malformed facts are refused and leave the last good link in place.
    let bad = map(&[("paired", Value::from("yes"))]);
    assert!(host
        .offload_advertise("com.example.offload", &bad)
        .await
        .is_err());
}

#[test]
fn offload_advertise_is_gated_on_detection_publish() {
    use crate::dispatch::{gate, Gate, Method};
    assert_eq!(
        gate("offload.advertise", false, &caps(&[])),
        Gate::CapabilityDenied("capability_denied: vision.detection.publish".to_string())
    );
    assert_eq!(
        gate(
            "offload.advertise",
            false,
            &caps(&["vision.detection.publish"])
        ),
        Gate::Allow(Method::OffloadAdvertise)
    );
}

// ---- node.info ----------------------------------------------------

fn now_unix() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

/// Every node-fact source under `dir`, with its production file name.
fn node_sources(dir: &std::path::Path) -> NodeInfoSources {
    NodeInfoSources {
        config_yaml: dir.join("config.yaml"),
        profile_conf: dir.join("profile.conf"),
        mesh_role: dir.join("mesh-role"),
        board_sidecar: dir.join("board.json"),
        camera_state: dir.join("camera-state.json"),
    }
}

/// A board sidecar as the HAL probe publishes it.
fn write_board(dir: &std::path::Path, npu_tops: f64, local_inference: &str) {
    let body = serde_json::json!({
        "version": 1, "name": "Radxa CM4 (RK3588S2)", "model": "Radxa CM4 IO Board",
        "tier": 4, "ram_mb": 8192, "cpu_cores": 8, "vendor": "Radxa", "soc": "RK3588S2",
        "arch": "aarch64", "hw_video_codecs": [], "npu_tops": npu_tops,
        "has_accelerator": npu_tops > 0.0, "local_inference": local_inference,
        "has_local_inference": local_inference != "none",
    });
    std::fs::write(dir.join("board.json"), body.to_string()).unwrap();
}

fn write_camera_state(dir: &std::path::Path, state: &str, pipeline: &str, age_s: f64) {
    let body = serde_json::json!({
        "version": 2, "state": state, "pipeline_state": pipeline,
        "updated_at_unix": now_unix() - age_s,
    });
    std::fs::write(dir.join("camera-state.json"), body.to_string()).unwrap();
}

async fn node_info(host: &RealHost) -> ados_protocol::node_info::NodeInfo {
    let reply = host.node_info("com.example.node", &map(&[])).await.unwrap();
    rmpv::ext::from_value(reply).expect("the reply decodes as the wire type")
}

#[tokio::test]
async fn node_info_reads_each_fact_from_its_source() {
    use ados_protocol::node_info::*;
    let dir = tempfile::tempdir().unwrap();
    // The primary leg of `video.cameras` overrides the legacy block, exactly as
    // the video service resolves the stream it encodes at `main`.
    std::fs::write(
        dir.path().join("config.yaml"),
        "agent:\n  profile: drone\nvideo:\n  camera: { width: 640, height: 480, fps: 15 }\n  \
         cameras:\n    - { id: ir, source: rtsp://pod/ir, role: ir, width: 320, height: 256 }\n    \
         - { id: eo, source: /dev/video0, role: primary, width: 1920, height: 1080, fps: 25 }\n",
    )
    .unwrap();
    write_board(dir.path(), 6.0, "onnx");
    write_camera_state(dir.path(), "ready", "streaming", 2.0);
    let host = RealHost::new().with_node_info_sources(node_sources(dir.path()));

    assert_eq!(
        node_info(&host).await,
        NodeInfo {
            profile: "drone".to_string(),
            board: Some(BoardInfo {
                id: "Radxa CM4 (RK3588S2)".to_string(),
                name: "Radxa CM4 IO Board".to_string(),
                has_npu: true,
                accelerators: vec!["npu".to_string(), "cpu-onnx".to_string()],
            }),
            ground_station: GroundStationInfo { role: None },
            camera: CameraInfo {
                ready: true,
                main: Some(StreamGeometry {
                    width: 1920,
                    height: 1080,
                    fps: 25,
                }),
            },
        }
    );

    // An NPU-less board: no accelerator, the fact the offload decision keys on.
    write_board(dir.path(), 0.0, "none");
    let board = node_info(&host).await.board.unwrap();
    assert!(!board.has_npu);
    assert!(board.accelerators.is_empty());

    // Ready means the pipeline is publishing `main`. A detected camera whose
    // encoder is spawned but has not published yet (a network source still
    // dialling its URL), a stopped or failed pipeline, and a report the video
    // service stopped re-stamping are all not ready.
    for pipeline in ["starting", "stopped", "error"] {
        write_camera_state(dir.path(), "ready", pipeline, 2.0);
        assert!(!node_info(&host).await.camera.ready, "{pipeline}");
    }
    write_camera_state(dir.path(), "ready", "streaming", 120.0);
    assert!(!node_info(&host).await.camera.ready);
}

#[tokio::test]
async fn node_info_on_a_ground_station_reports_its_role_and_no_camera_stream() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.yaml"), "agent:\n  profile: auto\n").unwrap();
    std::fs::write(dir.path().join("profile.conf"), "profile: ground_station\n").unwrap();
    std::fs::write(dir.path().join("mesh-role"), "relay\n").unwrap();
    let host = RealHost::new().with_node_info_sources(node_sources(dir.path()));

    let info = node_info(&host).await;
    assert_eq!(info.profile, "ground-station");
    assert_eq!(info.ground_station.role.as_deref(), Some("relay"));
    assert_eq!(info.camera.main, None);

    // No role sentinel: a ground station runs the direct plane.
    std::fs::remove_file(dir.path().join("mesh-role")).unwrap();
    assert_eq!(
        node_info(&host).await.ground_station.role.as_deref(),
        Some("direct")
    );
}

#[tokio::test]
async fn node_info_maps_every_absent_source_to_null() {
    let dir = tempfile::tempdir().unwrap();
    let host = RealHost::new().with_node_info_sources(node_sources(dir.path()));
    let reply = ok_map(host.node_info("com.example.node", &map(&[])).await);

    // Nothing on disk: the resolver's drone default, and an explicit nil for
    // every fact whose source is missing, never a fabricated board or stream.
    assert_eq!(
        field(&reply, "profile").and_then(Value::as_str),
        Some("drone")
    );
    assert_eq!(field(&reply, "board"), Some(&Value::Nil));
    let gs = field(&reply, "ground_station")
        .and_then(Value::as_map)
        .unwrap();
    assert_eq!(field(gs, "role"), Some(&Value::Nil));
    let camera = field(&reply, "camera").and_then(Value::as_map).unwrap();
    assert_eq!(field(camera, "ready"), Some(&Value::Boolean(false)));
    assert_eq!(field(camera, "main"), Some(&Value::Nil));

    // A board sidecar that is not the published contract is no board.
    std::fs::write(dir.path().join("board.json"), r#"{"name":"x"}"#).unwrap();
    assert_eq!(node_info(&host).await.board, None);
}

#[test]
fn node_info_is_gated_on_its_read_capability() {
    use crate::dispatch::{gate, Gate, Method};
    assert_eq!(
        gate("node.info", false, &caps(&[])),
        Gate::CapabilityDenied("capability_denied: node.info.read".to_string())
    );
    assert_eq!(
        gate("node.info", false, &caps(&["node.info.read"])),
        Gate::Allow(Method::NodeInfo)
    );
}

// ---- mdns.advertise / mdns.browse ------------------------------------

/// A workstation node running a plugin whose `node` service declares 8092 and
/// only runs on a workstation, plus an `edge` service declaring 9100 that only
/// runs on a drone.
fn mdns_host(dir: &std::path::Path) -> RealHost {
    let plugin_dir = dir.join("com.example.node");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    std::fs::write(
        plugin_dir.join("manifest.yaml"),
        "id: com.example.node\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\n\
         agent:\n  entrypoint: agent/py/x.py\n  permissions: [network.outbound, network.listen]\n  \
         contributes:\n    services:\n      - name: node\n        command: bin/node\n        \
         listen_ports: [8092]\n        profiles: [workstation]\n      - name: edge\n        \
         command: bin/edge\n        listen_ports: [9100]\n        profiles: [drone]\n",
    )
    .unwrap();
    std::fs::write(dir.join("profile.conf"), "profile: workstation\n").unwrap();
    RealHost::new()
        .with_node_info_sources(node_sources(dir))
        .with_runtime_lookup(Box::new(move |id| {
            (id == "com.example.node").then(|| (plugin_dir.clone(), BTreeSet::new()))
        }))
}

fn advertise_args(service_type: &str, port: u16) -> Value {
    map(&[
        ("service_type", Value::from(service_type)),
        ("port", Value::from(port)),
        (
            "txt",
            Value::Map(vec![(Value::from("deviceId"), Value::from("compute-1"))]),
        ),
    ])
}

/// A plugin advertises only what its sandbox lets it serve on this node: a
/// port no service declares, and a port declared only for a service that does
/// not run on this profile, are both refused before anything is published.
#[tokio::test]
async fn mdns_advertise_refuses_a_port_the_plugin_does_not_serve_here() {
    let dir = tempfile::tempdir().unwrap();
    let host = mdns_host(dir.path());
    for port in [9999, 9100] {
        let err = host
            .mdns_advertise(
                "com.example.node",
                1,
                &advertise_args("_ados-compute._tcp", port),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, HostError::Rpc(m) if m.contains("not a listen port")),
            "{port}: {err:?}"
        );
    }
    // A plugin with no manifest the host can find serves nothing.
    let err = host
        .mdns_advertise(
            "com.example.other",
            1,
            &advertise_args("_ados-compute._tcp", 8092),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(&err, HostError::Rpc(m) if m.contains("not a listen port")),
        "{err:?}"
    );
}

/// The agent's own records cannot be impersonated, and a malformed request is
/// refused with a reason.
#[tokio::test]
async fn mdns_advertise_refuses_the_agents_own_service_types() {
    let dir = tempfile::tempdir().unwrap();
    let host = mdns_host(dir.path());
    for ty in ["_ados._tcp", "_ados-receiver._tcp.local."] {
        let err = host
            .mdns_advertise("com.example.node", 1, &advertise_args(ty, 8092))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, HostError::Rpc(m) if m.contains("published by the agent")),
            "{ty}: {err:?}"
        );
    }
    let err = host
        .mdns_advertise("com.example.node", 1, &map(&[("port", Value::from(8092))]))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, HostError::Rpc(m) if m.contains("arguments are invalid")),
        "{err:?}"
    );
}

#[test]
fn mdns_methods_are_gated_on_the_network_capabilities() {
    use crate::dispatch::{gate, Gate, Method};
    assert_eq!(
        gate("mdns.advertise", false, &caps(&["network.outbound"])),
        Gate::CapabilityDenied("capability_denied: network.listen".to_string())
    );
    assert_eq!(
        gate("mdns.advertise", false, &caps(&["network.listen"])),
        Gate::Allow(Method::MdnsAdvertise)
    );
    assert_eq!(
        gate("mdns.browse", false, &caps(&["network.listen"])),
        Gate::CapabilityDenied("capability_denied: network.outbound".to_string())
    );
    assert_eq!(
        gate("mdns.browse", false, &caps(&["network.outbound"])),
        Gate::Allow(Method::MdnsBrowse)
    );
}
