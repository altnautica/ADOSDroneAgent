use super::*;
use serde_json::json;

#[test]
fn perception_fields_maps_board_caps_to_a_tier() {
    // An NPU board (npu_tops > 0) with a confirmed live backend runs
    // detection locally.
    let (npu, accel, tier, _target) = perception_fields(&json!({ "npu_tops": 6.0 }), true);
    assert_eq!(npu, 6.0);
    assert!(accel);
    assert_eq!(tier, "local");
    // A CPU board with no NPU + no paired node has no perception tier (no
    // offload-link sidecar in the test env ⇒ compute_node_paired false).
    let (npu, accel, tier, _target) = perception_fields(&json!({ "npu_tops": 0.0 }), true);
    assert_eq!(npu, 0.0);
    assert!(!accel);
    assert_eq!(tier, "none");
    // A board block with no npu_tops key degrades to no-accelerator/none.
    let (_npu, accel, tier, _target) = perception_fields(&json!({}), true);
    assert!(!accel);
    assert_eq!(tier, "none");
    // A CPU-strong board with no NPU but the profile-declared ONNX local
    // inference reads `local` (runs the detector on-board) when the live
    // backend confirms it, with the accelerator flag still false (it has
    // no NPU).
    let (npu, accel, tier, _target) = perception_fields(
        &json!({ "npu_tops": 0.0, "has_local_inference": true }),
        true,
    );
    assert_eq!(npu, 0.0);
    assert!(!accel);
    assert_eq!(tier, "local");
}

#[test]
fn perception_fields_never_claims_local_without_a_live_backend() {
    // A declared NPU (npu_tops > 0) with no in-tree
    // backend for it (e.g. the A733's VIP9000) resolves `select_backend`
    // to MockBackend, which loads every model and returns zero
    // detections. The declared capability must not outrun the live
    // signal — `has_accelerator` (the raw hardware fact) stays true, but
    // the tier degrades to `none` rather than lying `local`.
    let (npu, accel, tier, _target) = perception_fields(&json!({ "npu_tops": 3.0 }), false);
    assert_eq!(npu, 3.0);
    assert!(accel, "hasAccelerator is the raw hardware declaration");
    assert_eq!(tier, "none");
    // Same dishonesty for the CPU-ONNX declaration: a board profile can
    // declare `has_local_inference` while the deployed binary was not
    // built with the `onnx` feature, so no real backend loaded.
    let (_npu, _accel, tier, _target) = perception_fields(
        &json!({ "npu_tops": 0.0, "has_local_inference": true }),
        false,
    );
    assert_eq!(tier, "none");
    // An unreachable engine (vision disabled / not up) resolves the same
    // live signal to `false` upstream in `live_backend_inference_capable`,
    // so this case also covers "vision is off".
}

#[test]
fn telemetry_strips_the_four_runtime_extras() {
    let snapshot = json!({
        "armed": true,
        "mode": "GUIDED",
        "battery": {"voltage": 16.4},
        "fc_connected": true,
        "fc_port": "/dev/ttyACM0",
        "fc_baud": 115200,
        "service_uptime": 99.0,
    });
    let tel = project_telemetry(Some(snapshot));
    let obj = tel.as_object().unwrap();
    // The vehicle keys survive.
    assert!(obj.contains_key("armed"));
    assert!(obj.contains_key("mode"));
    assert!(obj.contains_key("battery"));
    // The four extras are gone.
    for k in IPC_ONLY_KEYS {
        assert!(!obj.contains_key(k), "{k} must be stripped from telemetry");
    }
}

#[test]
fn telemetry_of_an_absent_snapshot_is_an_empty_object() {
    assert_eq!(project_telemetry(None), json!({}));
}

#[test]
fn fc_triple_defaults_to_disconnected_when_the_snapshot_is_absent() {
    let (connected, port, baud, uptime) = fc_from_snapshot(None);
    assert_eq!(connected, json!(false));
    assert_eq!(port, json!(""));
    assert_eq!(baud, json!(0));
    assert!(uptime.is_none());
}

#[test]
fn fc_triple_reads_the_snapshot_extras() {
    let snap = json!({
        "fc_connected": true,
        "fc_port": "/dev/ttyACM0",
        "fc_baud": 115200,
        "service_uptime": 42.0,
    });
    let (connected, port, baud, uptime) = fc_from_snapshot(Some(&snap));
    assert_eq!(connected, json!(true));
    assert_eq!(port, json!("/dev/ttyACM0"));
    assert_eq!(baud, json!(115200));
    assert_eq!(uptime, Some(json!(42.0)));
}

#[test]
fn fc_liveness_defaults_when_the_snapshot_is_absent() {
    let l = fc_liveness_from_snapshot(None);
    assert!(!l.transport_open);
    assert!(!l.mavlink_alive);
    assert_eq!(l.heartbeat_age_s, Value::Null);
    assert_eq!(l.fc_source, json!("auto"));
    assert_eq!(l.fc_link_hint, json!("none"));
    // Absent field defaults to "not gated": an ordinary FC source is the case.
    assert!(!l.fc_command_down_gated);
    assert_eq!(l.fc_variant, Value::Null);
    // The firmware family is always a concrete string, defaulting to unknown.
    assert_eq!(l.fc_firmware, json!("unknown"));
}

#[test]
fn fc_liveness_surfaces_a_command_down_gated_source_honestly() {
    // A live MAVLink-over-ELRS ingest running telemetry-only: the link is
    // alive (telemetry flows) yet the host->FC command direction is gated
    // closed, so the surface must carry that fact — a bare connected boolean
    // would imply a GCS command reaches the FC when it is silently dropped.
    let snap = json!({
        "transport_open": true,
        "mavlink_alive": true,
        "fc_source": "serial",
        "fc_command_down_gated": true,
    });
    let l = fc_liveness_from_snapshot(Some(&snap));
    assert!(l.transport_open);
    assert!(l.mavlink_alive);
    assert!(
        l.fc_command_down_gated,
        "an alive telemetry-only source reports the command path gated"
    );
}

#[test]
fn fc_liveness_reads_the_snapshot_extras() {
    let snap = json!({
        "transport_open": true,
        "mavlink_alive": false,
        "heartbeat_age_s": 7.5,
        "fc_source": "serial",
        "fc_link_hint": "msp_detected",
        "fc_variant": "betaflight",
        "fc_firmware": "betaflight",
    });
    let l = fc_liveness_from_snapshot(Some(&snap));
    // The exact bug it guards: transport open but MAVLink not alive.
    assert!(l.transport_open);
    assert!(!l.mavlink_alive);
    assert_eq!(l.heartbeat_age_s, json!(7.5));
    assert_eq!(l.fc_source, json!("serial"));
    assert_eq!(l.fc_link_hint, json!("msp_detected"));
    assert_eq!(l.fc_variant, json!("betaflight"));
    assert_eq!(l.fc_firmware, json!("betaflight"));
}

#[test]
fn fc_reachable_is_true_for_a_live_mavlink_fc() {
    let l = fc_liveness_from_snapshot(Some(&json!({
        "transport_open": true,
        "mavlink_alive": true,
        "fc_firmware": "ardupilot",
    })));
    assert!(derive_fc_reachable(&l));
}

#[test]
fn fc_reachable_is_true_for_a_detected_msp_fc_on_an_open_port() {
    // Betaflight/iNav never heartbeats, but it is present + reachable: an
    // open transport plus the USB-descriptor variant is positive evidence.
    let by_variant = fc_liveness_from_snapshot(Some(&json!({
        "transport_open": true,
        "mavlink_alive": false,
        "fc_link_hint": "msp_detected",
        "fc_variant": "betaflight",
        "fc_firmware": "betaflight",
    })));
    assert!(derive_fc_reachable(&by_variant));

    // Or an unrecognised-descriptor MSP board evidenced by the frame sniff
    // alone (fc_variant null, hint msp_detected).
    let by_sniff = fc_liveness_from_snapshot(Some(&json!({
        "transport_open": true,
        "mavlink_alive": false,
        "fc_link_hint": "msp_detected",
    })));
    assert!(derive_fc_reachable(&by_sniff));
}

#[test]
fn fc_reachable_is_false_for_a_silent_open_port_and_for_no_transport() {
    // Port open, no heartbeat, no MSP evidence: nothing proves an FC is
    // there, so reachability must not be claimed.
    let silent = fc_liveness_from_snapshot(Some(&json!({
        "transport_open": true,
        "mavlink_alive": false,
        "fc_link_hint": "no_heartbeat",
    })));
    assert!(!derive_fc_reachable(&silent));

    // No transport at all.
    assert!(!derive_fc_reachable(&fc_liveness_from_snapshot(None)));
}

#[test]
fn telemetry_withholds_vehicle_fields_but_reports_agent_diagnostics_when_mavlink_not_alive() {
    // Transport open but the link is not alive: the vehicle fields are
    // stale defaults and must not surface, while the agent's own counters
    // stay true and are exactly what an operator needs in this state.
    let snapshot = json!({
        "armed": false,
        "mode": "STABILIZE",
        "battery": {"voltage": 0.0},
        "attitude": {"roll": 0.0, "pitch": 0.0, "yaw": 0.0},
        "params": {"SYSID_THISMAV": 1.0},
        "param_generation": 12,
        "video_profile": "thumbnail",
        "transport_open": true,
        "mavlink_alive": false,
        "fc_link_hint": "no_heartbeat",
        "fc_reachable": false,
        "ipc_state_drops": 4,
        "aux_rpc": {"rpc_requests": 12},
        "mavlink_frame_ingest": {"published": 900},
    });
    let tel = project_telemetry(Some(snapshot));
    let obj = tel.as_object().unwrap();

    for k in ["armed", "mode", "battery", "params"] {
        assert!(
            !obj.contains_key(k),
            "{k} is vehicle-sourced and must stay gated"
        );
    }
    // The cockpit SPA decides "is the vehicle live" from the presence of
    // `attitude` (cockpit/src/hooks/use-flight-telemetry.ts isLive), so this
    // key surfacing here would make a dead link render as flying. This
    // snapshot carries no relayed vehicle, so there is no honest reading to
    // put in its place — the relayed cases below cover the node that has
    // one.
    assert!(!obj.contains_key("attitude"));
    assert!(
        !obj.contains_key("telemetry_source"),
        "nothing is being relayed, so no provenance should be claimed"
    );
    for k in ["transport_open", "mavlink_alive", "fc_link_hint"] {
        assert!(!obj.contains_key(k), "{k} is never part of telemetry");
    }

    assert_eq!(obj["fc_reachable"], json!(false));
    assert_eq!(obj["ipc_state_drops"], json!(4));
    assert_eq!(obj["aux_rpc"], json!({"rpc_requests": 12}));
    assert_eq!(
        obj["mavlink_frame_ingest"],
        json!({"published": 900}),
        "a ground station never decodes a HEARTBEAT, so this is the only \
             state in which its ingest counters are ever readable"
    );
    // The parameter-cache change counter must survive the gate: it is the
    // agent's own bookkeeping, and withholding it would strand a consumer on a
    // stale parameter map for as long as the FC stayed down. A stray `params`
    // blob on the snapshot stays gated (asserted above) — the values are
    // vehicle-sourced and served by `GET /api/params`.
    assert_eq!(obj["param_generation"], json!(12));
    assert_eq!(
        obj["video_profile"],
        json!("thumbnail"),
        "the attention profile is an agent-owned RF allocation, true whether or \
             not an FC is attached"
    );
}

/// A ground-station snapshot: no local link, and a relayed vehicle whose
/// reading is `fresh`.
fn relayed_snapshot(fresh: bool) -> Value {
    json!({
        "transport_open": false,
        "mavlink_alive": false,
        "fc_connected": false,
        "fc_reachable": false,
        "mavlink_frame_ingest": {"frames_published": 395_111},
        "relayed_vehicle": {
            "source": "relayed",
            "fresh": fresh,
            "age_s": if fresh { 0.08 } else { 47.5 },
            "stale_after_s": 3.0,
            "system_id": 1,
            "frames_decoded": 12_345,
            "frames_undecodable": 0,
            "vehicle": {
                "armed": false,
                "mode": "STABILIZE",
                "attitude": {"roll": 0.088, "pitch": -0.037, "yaw": 0.39},
                "position": {"lat": 13.02, "lon": 77.65, "alt_rel": -16.34, "heading": 22.4},
                "battery": {"voltage": 12.4, "remaining": 29},
                "last_update": "2026-07-31T17:11:36+00:00",
            },
        },
    })
}

#[test]
fn a_fresh_relayed_vehicle_is_projected_in_place_of_the_withheld_local_fields() {
    // The headline fix. A ground station relaying a healthy aircraft used
    // to report no aircraft: the local gate blanked the vehicle fields and
    // nothing took their place, so the on-box cockpit drew a dead horizon
    // over a link carrying hundreds of thousands of frames.
    let tel = project_telemetry(Some(relayed_snapshot(true)));
    let obj = tel.as_object().unwrap();

    assert_eq!(obj["attitude"]["roll"], json!(0.088));
    assert_eq!(obj["position"]["alt_rel"], json!(-16.34));
    assert_eq!(obj["battery"]["remaining"], json!(29));
    assert_eq!(obj["mode"], json!("STABILIZE"));

    // Provenance is explicit, so no consumer can mistake this for a direct
    // link.
    assert_eq!(obj["telemetry_source"], json!("relayed"));
    assert_eq!(obj["relayed_link"]["fresh"], json!(true));
    assert_eq!(obj["relayed_link"]["system_id"], json!(1));

    // The raw producer block never surfaces; only the projection does.
    assert!(!obj.contains_key("relayed_vehicle"));

    // And the agent's own diagnostics still survive the gate.
    assert_eq!(
        obj["mavlink_frame_ingest"]["frames_published"],
        json!(395_111)
    );
}

#[test]
fn a_relayed_vehicle_never_makes_the_node_claim_a_flight_controller() {
    // The honesty contract this fix must not buy its way out of: the
    // vehicle is visible, not attached. These keys describe the LOCAL link
    // and are never part of the telemetry projection at all.
    let tel = project_telemetry(Some(relayed_snapshot(true)));
    let obj = tel.as_object().unwrap();
    for k in ["fc_connected", "mavlink_alive", "transport_open"] {
        assert!(
            !obj.contains_key(k),
            "{k} describes the local link and must never ride telemetry"
        );
    }
    // `/api/status` reads them straight off the snapshot, untouched by this
    // projection, so a relayed node still reports no FC of its own.
    let (connected, _, _, _) = fc_from_snapshot(Some(&relayed_snapshot(true)));
    assert_eq!(connected, json!(false));
}

#[test]
fn a_stale_relayed_vehicle_is_named_but_its_readings_are_withheld() {
    // A lane that has gone quiet must go blank rather than hold its last
    // attitude — the same reason the local gate exists. The lane is still
    // NAMED, because "the aircraft went quiet" and "there is no aircraft"
    // are different things an operator needs to tell apart.
    let tel = project_telemetry(Some(relayed_snapshot(false)));
    let obj = tel.as_object().unwrap();

    assert!(
        !obj.contains_key("attitude"),
        "a stale relayed reading rendered as live is the exact lie the gate exists to stop"
    );
    assert!(!obj.contains_key("position"));
    assert_eq!(obj["telemetry_source"], json!("relayed"));
    assert_eq!(obj["relayed_link"]["fresh"], json!(false));
    assert_eq!(obj["relayed_link"]["age_s"], json!(47.5));
}

#[test]
fn an_attached_flight_controller_is_unaffected_by_the_relayed_path() {
    // A drone with its own FC must be byte-identical to before: no
    // provenance stamp, no relayed block, vehicle fields straight through.
    let tel = project_telemetry(Some(json!({
        "mavlink_alive": true,
        "fc_connected": true,
        "armed": true,
        "attitude": {"roll": 0.1, "pitch": 0.2, "yaw": 0.3},
    })));
    let obj = tel.as_object().unwrap();

    assert_eq!(obj["attitude"]["roll"], json!(0.1));
    assert_eq!(obj["armed"], json!(true));
    assert!(
        !obj.contains_key("telemetry_source"),
        "a direct link stamps nothing, so the drone payload shape is unchanged"
    );
    assert!(!obj.contains_key("relayed_link"));
    assert!(!obj.contains_key("fc_connected"));
}

#[test]
fn a_live_local_link_wins_over_a_relayed_one_and_says_nothing_about_it() {
    // Both at once: this node's own flight controller is alive AND it is
    // relaying someone else's aircraft. The local reading wins, because the
    // projection has exactly one set of vehicle keys and the attached
    // vehicle is the one this node is flying.
    //
    // The consequence is deliberate and worth stating: the relay is not
    // mentioned at all here, so `telemetry_source` stays absent and the
    // `relayed_link` provenance is dropped. Telemetry answers "what is this
    // node's vehicle doing"; the relay's own health is reported by the
    // ingest counters, which do survive. Surfacing a second vehicle's
    // reading on this route would need a second set of keys, not a stamp on
    // this one.
    let tel = project_telemetry(Some(json!({
        "mavlink_alive": true,
        "fc_connected": true,
        "attitude": {"roll": 0.1, "pitch": 0.2, "yaw": 0.3},
        "mavlink_frame_ingest": {"frames_published": 12},
        "relayed_vehicle": {
            "source": "relayed",
            "fresh": true,
            "age_s": 0.05,
            "system_id": 7,
            "vehicle": {"attitude": {"roll": 9.9, "pitch": 9.9, "yaw": 9.9}},
        },
    })));
    let obj = tel.as_object().unwrap();

    // The local reading, not the relayed one.
    assert_eq!(obj["attitude"]["roll"], json!(0.1));
    assert!(!obj.contains_key("telemetry_source"));
    assert!(!obj.contains_key("relayed_link"));
    // The raw producer block never leaks either way.
    assert!(!obj.contains_key("relayed_vehicle"));
    // The relay's own health still reaches the operator.
    assert_eq!(obj["mavlink_frame_ingest"]["frames_published"], json!(12));
}

#[test]
fn a_snapshot_that_cannot_prove_the_link_is_down_projects_nothing_relayed() {
    // `mavlink_alive` absent means staleness cannot be PROVEN, so the local
    // fields pass through unchanged — the pre-existing contract. The relayed
    // block is still stripped rather than projected, because projecting it
    // would mean overwriting local readings this snapshot never said were
    // untrustworthy.
    //
    // In practice the router and this crate ship together and a ground
    // station always emits the flag, so this is the older-snapshot path. It
    // is pinned so the fallback stays a deliberate choice rather than an
    // accident of ordering.
    let tel = project_telemetry(Some(json!({
        "armed": false,
        "attitude": {"roll": 0.5, "pitch": 0.0, "yaw": 0.0},
        "relayed_vehicle": {
            "source": "relayed",
            "fresh": true,
            "vehicle": {"attitude": {"roll": 9.9, "pitch": 9.9, "yaw": 9.9}},
        },
    })));
    let obj = tel.as_object().unwrap();

    assert_eq!(
        obj["attitude"]["roll"],
        json!(0.5),
        "local fields pass through"
    );
    assert!(
        !obj.contains_key("relayed_vehicle"),
        "the raw block never surfaces"
    );
    assert!(!obj.contains_key("telemetry_source"));
    assert!(!obj.contains_key("relayed_link"));
}

#[test]
fn telemetry_with_no_diagnostics_is_still_empty_when_mavlink_not_alive() {
    // The degenerate snapshot keeps the old shape: nothing to report.
    let snapshot = json!({"armed": false, "mavlink_alive": false});
    assert_eq!(project_telemetry(Some(snapshot)), json!({}));
}

#[test]
fn an_agent_diagnostic_is_reported_on_a_live_link_too() {
    // The gate changes what is withheld when the link is down; it must not
    // change anything about the live case.
    let snapshot = json!({
        "armed": true,
        "mavlink_alive": true,
        "aux_rpc": {"rpc_requests": 3},
    });
    let tel = project_telemetry(Some(snapshot));
    let obj = tel.as_object().unwrap();
    assert_eq!(obj["armed"], json!(true));
    assert_eq!(obj["aux_rpc"], json!({"rpc_requests": 3}));
    assert!(!obj.contains_key("mavlink_alive"));
}

#[test]
fn the_uplink_conflict_counter_survives_a_down_link() {
    // A ground station has no flight controller of its own, so its link
    // always reads down; the count of commands it held must still show.
    let snapshot = json!({
        "armed": true,
        "mavlink_alive": false,
        "uplink_blocked_sysid_conflict": 4,
    });
    let tel = project_telemetry(Some(snapshot));
    let obj = tel.as_object().unwrap();
    assert!(!obj.contains_key("armed"));
    assert_eq!(obj["uplink_blocked_sysid_conflict"], json!(4));
}

#[test]
fn the_two_key_classifications_do_not_overlap() {
    // An IPC-only extra that also claimed to be a surfaced diagnostic would
    // appear when the FC is down and vanish when it is up.
    for k in AGENT_DIAGNOSTIC_KEYS {
        assert!(
            !IPC_ONLY_KEYS.contains(&k),
            "{k} cannot be both an IPC-only extra and a surfaced diagnostic"
        );
    }
}

#[test]
fn telemetry_passes_through_when_mavlink_alive() {
    // A live link surfaces the vehicle fields (minus the runtime-only extras).
    let snapshot = json!({
        "armed": true,
        "mode": "GUIDED",
        "mavlink_alive": true,
    });
    let tel = project_telemetry(Some(snapshot));
    let obj = tel.as_object().unwrap();
    assert_eq!(obj["armed"], json!(true));
    assert_eq!(obj["mode"], json!("GUIDED"));
    assert!(!obj.contains_key("mavlink_alive"));
}

#[test]
fn telemetry_strips_the_fc_liveness_extras() {
    let snapshot = json!({
        "armed": true,
        "transport_open": true,
        "mavlink_alive": true,
        "heartbeat_age_s": 0.5,
        "fc_source": "serial",
    });
    let tel = project_telemetry(Some(snapshot));
    let obj = tel.as_object().unwrap();
    assert!(obj.contains_key("armed"));
    for k in [
        "transport_open",
        "mavlink_alive",
        "heartbeat_age_s",
        "fc_source",
    ] {
        assert!(!obj.contains_key(k), "{k} must be stripped from telemetry");
    }
}

#[test]
fn fc_triple_treats_null_fields_as_the_defaults() {
    let snap = json!({
        "fc_connected": false,
        "fc_port": null,
        "fc_baud": null,
        "service_uptime": null,
    });
    let (connected, port, baud, uptime) = fc_from_snapshot(Some(&snap));
    assert_eq!(connected, json!(false));
    assert_eq!(port, json!(""));
    assert_eq!(baud, json!(0));
    assert!(uptime.is_none());
}

#[test]
fn binary_on_path_finds_a_known_command() {
    // `sh` is on PATH on every unix test host; a nonsense name is not.
    #[cfg(unix)]
    {
        assert!(binary_on_path("sh"), "sh should be on PATH");
    }
    assert!(!binary_on_path("definitely-not-a-real-binary-xyz"));
}

#[test]
fn process_uptime_is_non_negative() {
    let t = Instant::now();
    assert!(process_uptime_seconds(t) >= 0.0);
}

fn signals(pairs: &[(&str, Value)]) -> Map<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

#[test]
fn health_derives_the_five_fields_from_signals() {
    let s = signals(&[
        ("cpu.util.all", json!(12.34)),
        ("mem.total_bytes", json!(4000.0)),
        ("mem.avail_bytes", json!(1000.0)),
        ("disk.fs_total_bytes", json!(1000.0)),
        ("disk.fs_used_bytes", json!(250.0)),
        ("thermal.primary_c", json!(47.5)),
    ]);
    let h = derive_health(Some(&s));
    assert_eq!(h["cpu_percent"], json!(12.3)); // rounded to 1 decimal
    assert_eq!(h["memory_percent"], json!(75.0)); // (4000-1000)/4000*100
    assert_eq!(h["disk_percent"], json!(25.0)); // 250/1000*100
    assert_eq!(h["temperature"], json!(47.5));
    // The timestamp is a non-empty ISO string ending in the UTC offset.
    let ts = h["timestamp"].as_str().unwrap();
    assert!(ts.ends_with("+00:00") && ts.contains('T'), "ts: {ts}");
}

/// No signal source at all: every percentage is `null`, and the KEY is still
/// there.
///
/// This pinned `0.0` before, which is the defect: 0% CPU is a legal reading
/// for an idle node, so a consumer could not tell an unreporting agent from a
/// healthy one — and `/api/status/full` served exactly that on every stock
/// node. The key must survive serialization as an explicit null, because a
/// key that vanishes reads as "not reported" and gets coerced downstream.
#[test]
fn health_of_an_absent_store_reports_null_not_zero() {
    let h = derive_health(None);
    for field in [
        "cpu_percent",
        "memory_percent",
        "disk_percent",
        "temperature",
    ] {
        assert_eq!(h[field], Value::Null, "{field} must be null, not a number");
        assert!(
            h.as_object().unwrap().contains_key(field),
            "{field} must still be present as an explicit null"
        );
    }
    // Even with no store the shape carries all five keys.
    assert!(h["timestamp"].is_string());
}

/// A partially-sampled store reports the signals it has and nulls the rest,
/// per field — a measured 5% CPU beside an unmeasured memory reading.
#[test]
fn health_nulls_only_the_signals_that_are_missing() {
    // Signals present but neither the thermal zone nor memory sampled yet.
    let s = signals(&[("cpu.util.all", json!(5.0))]);
    let h = derive_health(Some(&s));
    assert_eq!(h["cpu_percent"], json!(5.0));
    assert_eq!(h["temperature"], Value::Null);
    assert_eq!(h["memory_percent"], Value::Null);
    assert_eq!(h["disk_percent"], Value::Null);
}

#[test]
fn read_board_returns_the_object_when_present() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("board.json");
    std::fs::write(&path, r#"{"name":"rpi4b","soc":"BCM2711","ram_mb":4096}"#).unwrap();
    let board = read_board(&path);
    assert_eq!(board["name"], json!("rpi4b"));
    assert_eq!(board["ram_mb"], json!(4096));
}

#[test]
fn read_board_of_an_absent_file_is_empty_object() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(read_board(&dir.path().join("nope.json")), json!({}));
}

#[test]
fn read_board_of_a_non_object_body_is_empty_object() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("board.json");
    std::fs::write(&path, "[1,2,3]").unwrap();
    assert_eq!(read_board(&path), json!({}));
}

#[test]
fn iso8601_formats_known_epochs() {
    // Epoch zero.
    assert_eq!(iso8601_from_unix_secs(0), "1970-01-01T00:00:00+00:00");
    // 2021-01-01T00:00:00Z = 1609459200.
    assert_eq!(
        iso8601_from_unix_secs(1_609_459_200),
        "2021-01-01T00:00:00+00:00"
    );
    // Noon on a leap day exercises the Feb-29 path: 2020-02-29T12:00:00Z =
    // 1582977600.
    assert_eq!(
        iso8601_from_unix_secs(1_582_977_600),
        "2020-02-29T12:00:00+00:00"
    );
    // One second before midnight, 2024-12-31T23:59:59Z = 1735689599.
    assert_eq!(
        iso8601_from_unix_secs(1_735_689_599),
        "2024-12-31T23:59:59+00:00"
    );
}
