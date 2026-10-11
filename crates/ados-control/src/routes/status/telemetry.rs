//! The telemetry projection and the FC-liveness derivation from a state snapshot.

use super::*;

/// Project a state snapshot into the vehicle-state dict: the runtime-only extras
/// in [`IPC_ONLY_KEYS`] are stripped, and when the FC link is not alive the
/// vehicle fields are withheld too, leaving only [`AGENT_DIAGNOSTIC_KEYS`] —
/// unless the node is relaying a vehicle, in which case the relayed reading is
/// projected in their place with explicit provenance. An absent or non-object
/// snapshot projects to `{}`.
///
/// Shared with the consolidated `/api/status/full` route, whose `telemetry` block
/// is the same vehicle-state projection.
pub(crate) fn project_telemetry(snapshot: Option<Value>) -> Value {
    match snapshot {
        Some(Value::Object(mut map)) => {
            // Honesty gate: when the FC link is explicitly not alive, the
            // vehicle fields are stale or default (0 alt, 0 battery, 360
            // heading, HDOP 655), so they must not surface as live. The agent's
            // own diagnostics are not vehicle readings and stay true either
            // way, so they survive. Absence of the flag (an older snapshot)
            // keeps the prior behavior: staleness cannot be proven, so the
            // fields pass through.
            let fc_down = map.get("mavlink_alive").and_then(Value::as_bool) == Some(false);
            // Lifted out before the filter because it is neither a vehicle
            // reading nor an agent diagnostic: it is the SOURCE of the former
            // on a node whose vehicle is across a radio. Removing it here also
            // means the raw nested block never surfaces on this route — only
            // the projection below, which carries its provenance explicitly.
            let relayed = map.remove("relayed_vehicle");
            let mut projected: Map<String, Value> = map
                .into_iter()
                .filter(|(k, _)| {
                    if fc_down {
                        AGENT_DIAGNOSTIC_KEYS.contains(&k.as_str())
                    } else {
                        !IPC_ONLY_KEYS.contains(&k.as_str())
                    }
                })
                .collect();

            // A ground station has no flight controller of its own, so the gate
            // above correctly blanks the local vehicle fields — but it used to
            // leave nothing behind, and a node relaying a healthy aircraft
            // reported no aircraft at all. That is what made the on-box cockpit
            // render a dead horizon over a working link. When a relayed reading
            // is present it is projected here instead, and only while fresh:
            // a lane that has gone quiet must go blank rather than hold its
            // last attitude, which is the same reason the local gate exists.
            //
            // `fc_connected` / `mavlink_alive` are NOT touched. They describe
            // the LOCAL link and stay false. A relayed vehicle is visible, not
            // attached, and `telemetry_source` is what tells the two apart.
            if fc_down {
                if let Some(block) = relayed.as_ref().and_then(Value::as_object) {
                    let fresh = block.get("fresh").and_then(Value::as_bool) == Some(true);
                    if fresh {
                        if let Some(vehicle) = block.get("vehicle").and_then(Value::as_object) {
                            for (k, v) in vehicle {
                                projected.insert(k.clone(), v.clone());
                            }
                        }
                    }
                    // Stamped whether or not the reading is fresh: a stale lane
                    // that is KNOWN to exist is a different, more useful signal
                    // than no lane at all, and the operator should be able to
                    // tell "the aircraft went quiet" from "there is no
                    // aircraft".
                    projected.insert("telemetry_source".into(), json!("relayed"));
                    projected.insert("relayed_link".into(), relayed_link_summary(block));
                }
            }
            Value::Object(projected)
        }
        _ => json!({}),
    }
}

/// The compact provenance block published alongside a relayed reading.
///
/// Deliberately a summary rather than the producer's whole snapshot: consumers
/// need to know where the reading came from and whether to trust it, not the
/// internals of the seam that decoded it.
pub(super) fn relayed_link_summary(block: &Map<String, Value>) -> Value {
    let mut out = Map::new();
    for key in [
        "fresh",
        "age_s",
        "stale_after_s",
        "system_id",
        "frames_decoded",
        "frames_undecodable",
    ] {
        if let Some(v) = block.get(key) {
            out.insert(key.to_string(), v.clone());
        }
    }
    Value::Object(out)
}

/// Read the FC connection triple + the service uptime out of a snapshot's runtime
/// extras. Returns `(fc_connected, fc_port, fc_baud, uptime?)`. An absent snapshot
/// or absent fields fall back to the FastAPI defaults: `fc_connected:false`,
/// `fc_port:""`, `fc_baud:0`, and no uptime (the caller substitutes the process
/// uptime).
///
/// Shared with the consolidated `/api/status/full` route, which reads the same FC
/// triple + uptime from the same snapshot extras.
pub(crate) fn fc_from_snapshot(snapshot: Option<&Value>) -> (Value, Value, Value, Option<Value>) {
    let obj = snapshot.and_then(Value::as_object);
    let connected = obj
        .and_then(|m| m.get("fc_connected"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let port = obj
        .and_then(|m| m.get("fc_port"))
        .cloned()
        .filter(|v| !v.is_null())
        .unwrap_or_else(|| json!(""));
    let baud = obj
        .and_then(|m| m.get("fc_baud"))
        .cloned()
        .filter(|v| !v.is_null())
        .unwrap_or_else(|| json!(0));
    let uptime = obj
        .and_then(|m| m.get("service_uptime"))
        .cloned()
        .filter(|v| !v.is_null());
    (json!(connected), port, baud, uptime)
}

/// The FC-liveness detail the GCS lane reads, split out of the snapshot extras
/// the MAVLink router publishes. `transport_open` is whether the FC transport is
/// open; `mavlink_alive` is whether a fresh HEARTBEAT decoded within the router's
/// timeout; `heartbeat_age_s` is the seconds since the last HEARTBEAT (`null`
/// when none yet); `fc_source` is the configured transport class (`auto` /
/// `serial` / `udp` / `tcp`). Together they let the GCS show "port open · no
/// MAVLink" distinctly from connected, and validate the link is actually live.
pub(crate) struct FcLiveness {
    pub transport_open: bool,
    pub mavlink_alive: bool,
    pub heartbeat_age_s: Value,
    pub fc_source: Value,
    /// The not-alive diagnostic hint: `msp_detected` (the FC speaks MSP, not
    /// MAVLink, on this port), `no_heartbeat` (open but silent), or `none`.
    pub fc_link_hint: Value,
    /// Whether the open FC source is the MAVLink-over-ELRS ingest running
    /// telemetry-only with its host->FC command-down direction gated closed.
    /// True means the link can read connected (telemetry flows) yet a GCS
    /// command is silently dropped — so a consumer surfaces "commands gated"
    /// rather than implying the command reached the FC. False for every
    /// ordinary FC source, whose command path is open.
    pub fc_command_down_gated: bool,
    /// The FC firmware family from the USB descriptor (`betaflight`/`inav`), or
    /// null for a MAVLink/unknown FC. Lets the GCS badge an MSP FC honestly.
    pub fc_variant: Value,
    /// The canonical FC firmware family: `ardupilot`/`px4` (MAVLink, from the
    /// decoded HEARTBEAT.autopilot discriminator) or `betaflight`/`inav` (MSP,
    /// from the USB descriptor), or `unknown` when no verified FC identity is
    /// available. Unlike `fc_variant` it distinguishes the MAVLink pair
    /// ArduPilot vs PX4, so a consumer can badge all four families.
    pub fc_firmware: Value,
}

/// Read the FC-liveness extras out of a snapshot. An absent snapshot or absent
/// fields fall back to the safe defaults: transport closed, not alive, no
/// heartbeat age, source `"auto"` (the config default). Shared with
/// `/api/status/full`.
pub(crate) fn fc_liveness_from_snapshot(snapshot: Option<&Value>) -> FcLiveness {
    let obj = snapshot.and_then(Value::as_object);
    FcLiveness {
        transport_open: obj
            .and_then(|m| m.get("transport_open"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        mavlink_alive: obj
            .and_then(|m| m.get("mavlink_alive"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        heartbeat_age_s: obj
            .and_then(|m| m.get("heartbeat_age_s"))
            .cloned()
            .filter(|v| !v.is_null())
            .unwrap_or(Value::Null),
        fc_source: obj
            .and_then(|m| m.get("fc_source"))
            .cloned()
            .filter(|v| !v.is_null())
            .unwrap_or_else(|| json!("auto")),
        fc_link_hint: obj
            .and_then(|m| m.get("fc_link_hint"))
            .cloned()
            .filter(|v| !v.is_null())
            .unwrap_or_else(|| json!("none")),
        // Absent field (an older router snapshot) is honestly "not gated": an
        // ordinary FC source is the overwhelming case, and only a resolved
        // MAVLink-over-ELRS ingest is ever gated.
        fc_command_down_gated: obj
            .and_then(|m| m.get("fc_command_down_gated"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        fc_variant: obj
            .and_then(|m| m.get("fc_variant"))
            .cloned()
            .filter(|v| !v.is_null())
            .unwrap_or(Value::Null),
        // Always a concrete family string; an absent snapshot / field is
        // honestly `unknown`, never guessed.
        fc_firmware: obj
            .and_then(|m| m.get("fc_firmware"))
            .cloned()
            .filter(|v| !v.is_null())
            .unwrap_or_else(|| json!("unknown")),
    }
}

/// A truthful "the FC is reachable" signal. Unlike the heartbeat-gated
/// `fc_connected`, this is also true for a detected MSP FC on an open transport:
/// a Betaflight/iNav board never emits a MAVLink heartbeat, yet it is present
/// and reachable over the byte-pipe (identified by its USB descriptor or a
/// recent MSP frame sniff). It is false when the port is open but no FC is
/// evidenced (no heartbeat and no MSP), and when there is no transport at all —
/// it never claims a link that is not there.
pub(crate) fn derive_fc_reachable(liveness: &FcLiveness) -> bool {
    liveness.mavlink_alive
        || (liveness.transport_open
            && (!liveness.fc_variant.is_null() || liveness.fc_link_hint == json!("msp_detected")))
}
