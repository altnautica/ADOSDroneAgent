use super::*;
use crate::config::MavlinkConfig;
use crate::daemon::startup::proxy_posture;

/// Every key `build_extras` can emit, sorted.
///
/// `ados-control` sorts each of these into exactly one of four dispositions —
/// `IPC_ONLY_KEYS` (stripped from telemetry), `AGENT_DIAGNOSTIC_KEYS`
/// (survives the FC-down honesty gate), neither (vehicle-sourced, so gated),
/// or consumed-and-transformed (removed before the filter and re-emitted in
/// another shape) — and it cannot depend on this crate to verify its list is
/// complete. Pinning the set here is that seam: add an extra without
/// classifying it over there and this test fails.
///
/// `relayed_vehicle` is the only key in the fourth disposition today:
/// `project_telemetry` removes it and, when it is fresh, projects its nested
/// vehicle fields up in place of the withheld local ones. So it appears here
/// but in neither classification list, which is correct rather than an
/// omission.
const EXPECTED_EXTRAS_KEYS: [&str; 38] = [
    "agent_commands_unsigned_rejected_risk",
    "agent_signing",
    "aux_mavlink_tee",
    "aux_rpc",
    "aux_uplink_partial_dropped",
    "aux_uplink_queue_full_dropped",
    "fc_baud",
    "fc_command_down_gated",
    "fc_connected",
    "fc_device",
    "fc_firmware",
    "fc_frames_decoded",
    "fc_link_hint",
    "fc_port",
    "fc_reachable",
    "fc_source",
    "fc_variant",
    "heartbeat_age_s",
    "injector_arbitration",
    "ipc_mavlink_drops",
    "ipc_state_drops",
    "mavlink_alive",
    "mavlink_frame_ingest",
    "mavlink_proxy_posture",
    "param_cached_count",
    "param_expected_count",
    "param_generation",
    "param_priming",
    "param_sweep_send_failed",
    "param_sweep_timed_out",
    "relayed_vehicle",
    "service_uptime",
    "swarm_emergency",
    "swarm_precedence",
    "transport_open",
    "uplink_blocked_sysid_conflict",
    "video_profile",
    "visor_flight_frames_dropped",
];

#[tokio::test]
async fn build_extras_emits_exactly_the_classified_key_set() {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(Mutex::new(VehicleState::default()));
    let params = Arc::new(Mutex::new(ParamCache::new(dir.path().join("params.json"))));
    let fc = FcConnection::new(MavlinkConfig::default(), state.clone(), params.clone());

    // Every optional counter present at once. No single profile does this
    // (tee + rpc are drone-only, ingest is ground-station-only), but the pin
    // is about the key SET, not about one profile's subset of it.
    //
    // The relayed projection is POPULATED rather than default: an untouched
    // one has never seen a frame, so `to_wire` returns `None` and the key is
    // absent by design. Passing a default here would have pinned a key set
    // that silently omits the relayed lane — which is exactly the shape of
    // the bug this seam exists to catch.
    let relayed = relayed_with_one_frame();
    let extras = build_extras(
        &fc,
        &state,
        &params,
        Instant::now(),
        0,
        0,
        Some(&Arc::new(TeeCounters::default())),
        Some(&Arc::new(IngestCounters::default())),
        Some(&AuxUplinkConsumerCounters::new()),
        Some(&Arc::new(SwarmSetpointStatus::default())),
        Some(&aux_uplink::spawn(0)),
        Some(&relayed),
        &proxy_posture(&MavlinkConfig::default()),
    )
    .await;

    let mut keys: Vec<&str> = extras.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys, EXPECTED_EXTRAS_KEYS,
        "a snapshot key changed; classify it in ados-control's IPC_ONLY_KEYS \
         or AGENT_DIAGNOSTIC_KEYS (or neither, if it is vehicle-sourced, or \
         consumed-and-transformed like relayed_vehicle) before updating this pin"
    );
}

#[tokio::test]
async fn extras_carry_latched_identity() {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(Mutex::new(VehicleState::default()));
    let params = Arc::new(Mutex::new(ParamCache::new(dir.path().join("params.json"))));
    let fc = FcConnection::new(MavlinkConfig::default(), state.clone(), params.clone());
    let posture = proxy_posture(&MavlinkConfig::default());
    let before = build_extras(
        &fc,
        &state,
        &params,
        Instant::now(),
        0,
        0,
        None,
        None,
        None,
        None,
        None,
        None,
        &posture,
    )
    .await;
    assert!(!before.contains_key("fc_system_id"));
    assert!(!before.contains_key("fc_component_id"));

    fc.latch_fc_identity(7, 1);
    let after = build_extras(
        &fc,
        &state,
        &params,
        Instant::now(),
        0,
        0,
        None,
        None,
        None,
        None,
        None,
        None,
        &posture,
    )
    .await;
    assert_eq!(after["fc_system_id"], 7);
    assert_eq!(after["fc_component_id"], 1);
}

/// A relayed projection that has decoded exactly one frame.
///
/// Built through the public `apply_frame` rather than by reaching into the
/// type, so this helper exercises the same path production does.
fn relayed_with_one_frame() -> Arc<StdMutex<RelayedVehicle>> {
    use ados_protocol::mavlink::ardupilotmega::{
        MavAutopilot, MavModeFlag, MavState, MavType, HEARTBEAT_DATA,
    };
    use ados_protocol::mavlink::{serialize_v2, MavHeader, MavMessage};
    let msg = MavMessage::HEARTBEAT(HEARTBEAT_DATA {
        custom_mode: 0,
        mavtype: MavType::MAV_TYPE_QUADROTOR,
        autopilot: MavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA,
        base_mode: MavModeFlag::empty(),
        system_status: MavState::MAV_STATE_STANDBY,
        mavlink_version: 3,
    });
    let frame = serialize_v2(
        MavHeader {
            system_id: 1,
            component_id: 1,
            sequence: 0,
        },
        &msg,
    )
    .unwrap();
    let relayed = RelayedVehicle::default();
    let relayed = Arc::new(StdMutex::new(relayed));
    relayed
        .lock()
        .unwrap()
        .apply_frame(&frame, "2026-08-01T00:00:00+00:00", Instant::now());
    relayed
}

/// The producer's half of the cross-crate contract: the key `ados-control`
/// reads is the key this crate writes.
///
/// Worth its own test because the two live in different crates and the
/// consumer was merged first — for a while `project_telemetry` read
/// `relayed_vehicle` off every snapshot while nothing emitted it, so its own
/// tests passed against hand-built input and production never fired. A
/// spelling change on either side has to fail somewhere, and this is where.
#[tokio::test]
async fn a_populated_relayed_projection_reaches_the_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(Mutex::new(VehicleState::default()));
    let params = Arc::new(Mutex::new(ParamCache::new(dir.path().join("params.json"))));
    let fc = FcConnection::new(MavlinkConfig::default(), state.clone(), params.clone());
    let relayed = relayed_with_one_frame();

    let extras = build_extras(
        &fc,
        &state,
        &params,
        Instant::now(),
        0,
        0,
        None,
        Some(&Arc::new(IngestCounters::default())),
        None,
        None,
        None,
        Some(&relayed),
        &ProxyPosture::default(),
    )
    .await;

    let block = extras
        .get("relayed_vehicle")
        .expect("a populated relayed projection must reach the snapshot");
    assert_eq!(block["source"], "relayed");
    assert_eq!(block["fresh"], true);
    assert_eq!(block["frames_decoded"], 1);
    // Nested, never merged: the relayed reading must not be confusable with
    // the attached-FC fields sitting beside it in the same map.
    assert!(block["vehicle"].is_object());
    assert!(!extras.contains_key("attitude"));
}

/// A node that has never relayed a vehicle emits no key at all.
///
/// Absent and present-but-empty are different signals: the first says this
/// node has never seen a vehicle, the second would say it has one reading
/// all zeroes. Rendering the second as an aircraft is the failure mode.
#[tokio::test]
async fn a_node_that_never_relayed_emits_no_relayed_key() {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(Mutex::new(VehicleState::default()));
    let params = Arc::new(Mutex::new(ParamCache::new(dir.path().join("params.json"))));
    let fc = FcConnection::new(MavlinkConfig::default(), state.clone(), params.clone());
    let never = Arc::new(StdMutex::new(RelayedVehicle::default()));

    let extras = build_extras(
        &fc,
        &state,
        &params,
        Instant::now(),
        0,
        0,
        None,
        Some(&Arc::new(IngestCounters::default())),
        None,
        None,
        None,
        Some(&never),
        &ProxyPosture::default(),
    )
    .await;

    assert!(!extras.contains_key("relayed_vehicle"));
}

/// Serialized size of an extras map with the WALL-CLOCK keys removed.
///
/// `service_uptime` and `heartbeat_age_s` are floats formatted from
/// `Instant::now()`, so their digit count differs between two calls made
/// microseconds apart and the raw byte delta jitters by a couple of bytes.
/// Measuring the payload-diet property against a clock made this assertion
/// flaky by construction; the property itself is about the parameter cache, so
/// the clocks are simply not part of it.
fn sized_without_clocks(mut extras: Map<String, Value>) -> usize {
    extras.remove("service_uptime");
    extras.remove("heartbeat_age_s");
    serde_json::to_vec(&Value::Object(extras)).unwrap().len()
}

/// The payload-diet proof for the `params` blob removal.
///
/// The blob rode the 10 Hz state publish in full, so the snapshot grew
/// linearly with the FC's parameter count — ~24 KB against ArduPilot's ~700
/// parameters, which is ~21 aux fragments on a relayed read and the measured
/// 85% delivery. This measures both shapes against one populated cache: the
/// current extras, and the same extras with the blob put back. It fails on
/// the plausible bug (re-adding the map, or letting the counter carry values)
/// because the current shape's size must not move with the cache size.
#[tokio::test]
async fn extras_size_does_not_grow_with_the_cached_parameter_count() {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(Mutex::new(VehicleState::default()));
    let params = Arc::new(Mutex::new(ParamCache::new(dir.path().join("params.json"))));
    let fc = FcConnection::new(MavlinkConfig::default(), state.clone(), params.clone());

    let empty = build_extras(
        &fc,
        &state,
        &params,
        Instant::now(),
        0,
        0,
        Some(&Arc::new(TeeCounters::default())),
        Some(&Arc::new(IngestCounters::default())),
        Some(&AuxUplinkConsumerCounters::new()),
        None,
        None,
        None,
        &ProxyPosture::default(),
    )
    .await;
    let empty_len = sized_without_clocks(empty);

    // An ArduPilot-scale parameter set: 700 names of realistic length.
    {
        let mut pc = params.lock().await;
        for i in 0..700 {
            pc.set(&format!("ATC_RAT_RLL_P_{i:03}"), 0.135 + i as f64, 9);
        }
    }
    let full = build_extras(
        &fc,
        &state,
        &params,
        Instant::now(),
        0,
        0,
        Some(&Arc::new(TeeCounters::default())),
        Some(&Arc::new(IngestCounters::default())),
        Some(&AuxUplinkConsumerCounters::new()),
        None,
        None,
        None,
        &ProxyPosture::default(),
    )
    .await;

    // The pre-diet shape: the same extras carrying the whole `{name: value}`
    // map, which is exactly what `params` used to insert.
    let mut pre_diet = full.clone();
    // (`full` is consumed by the measurement below, so the variant is built first.)
    let blob: Map<String, Value> = (0..700)
        .map(|i| (format!("ATC_RAT_RLL_P_{i:03}"), json!(0.135 + i as f64)))
        .collect();
    pre_diet.insert("params".into(), Value::Object(blob));

    let full_len = sized_without_clocks(full);
    let pre_diet_len = sized_without_clocks(pre_diet);
    println!(
        "extras JSON bytes: {empty_len} empty cache, {full_len} with 700 params, \
         {pre_diet_len} with the pre-diet `params` blob"
    );

    // The counter is a scalar, so 700 parameters may only widen the snapshot
    // by the counter's own digits.
    assert!(
        full_len - empty_len <= 4,
        "extras grew {} bytes for 700 cached params; the map is back on the \
         10 Hz publish",
        full_len - empty_len
    );
    // And the shape it replaced was an order of magnitude larger. Measured
    // as the BLOB'S OWN weight, not as a ratio of the two totals: a ratio
    // silently pins the size of everything else in the snapshot, so adding
    // any unrelated scalar counter breaks it and tempts whoever hits that
    // into re-pinning the number rather than reading the assertion.
    assert!(
        pre_diet_len - full_len > 15_000,
        "the `params` blob only accounts for {} B; this guard is measuring \
         the wrong thing",
        pre_diet_len - full_len
    );
}

/// The posture reports each edge's live bind, not the configured address:
/// before any proxy has bound, every edge reads not-listening.
#[test]
fn posture_reports_the_listening_flags() {
    let posture = proxy_posture(&MavlinkConfig::default());
    let v = posture.to_value();
    assert_eq!(v["tcp_listening"], json!(false));
    let udp = v["udp_listening"].as_array().expect("one flag per port");
    assert_eq!(udp.len(), posture.udp.len());
    assert!(udp.iter().all(|b| *b == json!(false)));
    // The default config enables the WebSocket endpoint.
    assert_eq!(v["ws_listening"], json!(false));
    assert!(v.get("raw_bind").is_some());
}
