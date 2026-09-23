//! MAVLink v2 signing capability + state read routes.
//!
//! The agent never holds a signing key. These read routes let the GCS detect
//! whether the connected FC supports MAVLink v2 signing and read the
//! observational signed-frame counters. Capability reads the vehicle-state
//! snapshot the MAVLink service publishes on `/run/ados/state.sock` (held by the
//! [`StateIpcClient`](crate::ipc)):
//!
//! - **`/api/mavlink/signing/capability`** runs the capability check over the FC
//!   connection flag and the autopilot id from the snapshot. ArduPilot keeps the
//!   signing key in its own persistent store, written with `SETUP_SIGNING`, and
//!   exposes no `SIGNING_*` parameter, so support is decided by the autopilot
//!   family, not by the param tree. Any other firmware reports unsupported with a
//!   specific reason enum.
//! - **`/api/mavlink/signing/counters`** reports signed-frame counters. The agent
//!   validates nothing (it holds no key); the counters would only confirm signed
//!   frames are transiting. No process on the node observes frames for signing
//!   today, so nothing is measured and the route says so: `observed: false` with
//!   every count `null`. A zero would read as "measured, and no signed frame was
//!   seen", which is a different and usually false claim.
//!
//! There is no signing-enforcement flag to read or set: ArduPilot has no
//! `SIGNING_*` parameter. Once its store holds a key it rejects unsigned traffic
//! on every link but channel 0 (normally USB), and it reports nothing about it.
//!
//! With no agent running (an empty snapshot), capability reports
//! `fc_not_connected` and counters report unmeasured, each a valid,
//! GCS-parseable body rather than a failure.

use axum::extract::State;
use axum::Json;
use serde_json::{json, Value};

use crate::state::AppState;

/// MAV_AUTOPILOT enum values the capability check distinguishes. ArduPilot is the
/// only firmware that gets the persistent signing store the GCS targets; PX4 and
/// the heartbeat-only / non-MAVLink case each get a specific reason. Mirrors the
/// `mavutil.mavlink.MAV_AUTOPILOT_*` constants the Python check reads.
const MAV_AUTOPILOT_ARDUPILOTMEGA: i64 = 3;
const MAV_AUTOPILOT_INVALID: i64 = 8;
const MAV_AUTOPILOT_PX4: i64 = 12;

/// `GET /api/mavlink/signing/capability` → whether the connected FC supports
/// MAVLink v2 signing.
///
/// Reads the FC connection flag and the autopilot id from the live state snapshot
/// and runs the capability check. Returns
/// `{supported, reason, firmware_name, firmware_version, signing_params_present}`.
/// `signing_params_present` is informational only (whether the cached param tree
/// carries any `SIGNING_*` name); it does not gate support. The `reason` enum:
/// `ok | fc_not_connected | firmware_not_supported |
/// firmware_px4_no_persistent_store | msp_protocol`. An absent snapshot reads as
/// disconnected → `fc_not_connected`. Guaranteed-200, never 500.
pub async fn capability(State(state): State<AppState>) -> Json<Value> {
    let snapshot = state.state.snapshot();
    let connected = fc_connected_from_snapshot(snapshot.as_ref());
    let autopilot = autopilot_from_snapshot(snapshot.as_ref());
    let params = crate::param_store::read_param_blob(&state.params_path);
    Json(detect_capability(
        connected,
        autopilot,
        signing_params_present(&params),
    ))
}

/// `GET /api/mavlink/signing/counters` → the observational signed-frame counters.
///
/// The agent holds no signing key and validates nothing. No process on the node
/// observes frames for signing, so nothing is measured: `{observed: false,
/// tx_signed_count: null, rx_signed_count: null, last_signed_rx_at: null}`.
pub async fn counters() -> Json<Value> {
    Json(json!({
        "observed": false,
        "tx_signed_count": Value::Null,
        "rx_signed_count": Value::Null,
        "last_signed_rx_at": Value::Null,
    }))
}

/// Read the FC connection flag out of a snapshot. Absent snapshot or absent flag
/// reads as disconnected, mirroring the Python `_fc_connected` (which reads the
/// state snapshot's `fc_connected`).
fn fc_connected_from_snapshot(snapshot: Option<&Value>) -> bool {
    snapshot
        .and_then(Value::as_object)
        .and_then(|m| m.get("fc_connected"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Read the MAV_AUTOPILOT id out of a snapshot's `autopilot` field. Absent or
/// non-numeric reads as `0`, mirroring the Python `_autopilot` (`int(ipc.get(
/// "autopilot", 0) or 0)`). A JSON `0`, a `null`, and an absent key all collapse
/// to `0` (the `or 0` in Python turns a falsy `0`/`None` into `0`).
fn autopilot_from_snapshot(snapshot: Option<&Value>) -> i64 {
    snapshot
        .and_then(Value::as_object)
        .and_then(|m| m.get("autopilot"))
        .and_then(Value::as_i64)
        .unwrap_or(0)
}

/// Whether any `SIGNING_*` param is present in the cached param map. Reported as
/// information only: ArduPilot's signing store is not a parameter, so a real
/// ArduPilot FC normally reports `false` here and is still supported.
fn signing_params_present(params: &serde_json::Map<String, Value>) -> bool {
    params.keys().any(|name| name.starts_with("SIGNING_"))
}

/// The capability check. The FC must be connected and the autopilot must be
/// ArduPilot, whose `SETUP_SIGNING` store is persistent. PX4, the MSP path and any
/// other autopilot report unsupported with a specific reason enum.
fn detect_capability(connected: bool, autopilot: i64, signing_params_present: bool) -> Value {
    if !connected {
        return json!({
            "supported": false,
            "reason": "fc_not_connected",
            "firmware_name": Value::Null,
            "firmware_version": Value::Null,
            "signing_params_present": false,
        });
    }

    let firmware_name = autopilot_name(autopilot);

    if autopilot != MAV_AUTOPILOT_ARDUPILOTMEGA {
        // An autopilot that isn't ArduPilot: PX4 has no persistent signing store,
        // the heartbeat-only / non-MAVLink case is the MSP path, anything else is
        // simply unsupported. (autopilot == 0 here means no heartbeat yet, which
        // is already covered above by the fc_not_connected branch.)
        let reason = if autopilot == MAV_AUTOPILOT_PX4 {
            "firmware_px4_no_persistent_store"
        } else if autopilot == MAV_AUTOPILOT_INVALID {
            "msp_protocol"
        } else {
            "firmware_not_supported"
        };
        return json!({
            "supported": false,
            "reason": reason,
            "firmware_name": firmware_name,
            "firmware_version": Value::Null,
            "signing_params_present": signing_params_present,
        });
    }

    json!({
        "supported": true,
        "reason": "ok",
        "firmware_name": firmware_name,
        // The GCS populates firmware_version when it reads AUTOPILOT_VERSION.
        "firmware_version": Value::Null,
        "signing_params_present": signing_params_present,
    })
}

/// The display name for a MAV_AUTOPILOT id, mirroring the Python `_autopilot_name`.
/// `0` → `null` (no heartbeat received yet); ArduPilot / PX4 / the heartbeat-only
/// case get their names; any other id gets the `Autopilot-<id>` form.
fn autopilot_name(autopilot: i64) -> Value {
    match autopilot {
        0 => Value::Null,
        MAV_AUTOPILOT_ARDUPILOTMEGA => json!("ArduPilot"),
        MAV_AUTOPILOT_PX4 => json!("PX4"),
        MAV_AUTOPILOT_INVALID => json!("Unknown (non-MAVLink or heartbeat-only)"),
        other => json!(format!("Autopilot-{other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── capability: the golden parity fixtures ───────────────────────────────

    #[test]
    fn capability_disconnected_is_the_fc_not_connected_default() {
        // The exact Python JSON for the disconnected case (no snapshot).
        let got = detect_capability(false, 0, false);
        assert_eq!(
            got,
            json!({
                "supported": false,
                "reason": "fc_not_connected",
                "firmware_name": Value::Null,
                "firmware_version": Value::Null,
                "signing_params_present": false,
            })
        );
    }

    #[test]
    fn capability_ardupilot_is_supported_without_signing_params() {
        // A real ArduPilot FC carries no SIGNING_* param: its key store is written
        // with SETUP_SIGNING. Support must not hinge on the param tree.
        let got = detect_capability(true, MAV_AUTOPILOT_ARDUPILOTMEGA, false);
        assert_eq!(
            got,
            json!({
                "supported": true,
                "reason": "ok",
                "firmware_name": "ArduPilot",
                "firmware_version": Value::Null,
                "signing_params_present": false,
            })
        );
    }

    #[test]
    fn capability_ardupilot_reports_signing_params_as_information() {
        let got = detect_capability(true, MAV_AUTOPILOT_ARDUPILOTMEGA, true);
        assert_eq!(got["supported"], json!(true));
        assert_eq!(got["signing_params_present"], json!(true));
    }

    #[test]
    fn capability_px4_has_no_persistent_store() {
        let got = detect_capability(true, MAV_AUTOPILOT_PX4, true);
        assert_eq!(
            got,
            json!({
                "supported": false,
                "reason": "firmware_px4_no_persistent_store",
                "firmware_name": "PX4",
                "firmware_version": Value::Null,
                "signing_params_present": true,
            })
        );
    }

    #[test]
    fn capability_invalid_autopilot_is_the_msp_path() {
        let got = detect_capability(true, MAV_AUTOPILOT_INVALID, false);
        assert_eq!(
            got,
            json!({
                "supported": false,
                "reason": "msp_protocol",
                "firmware_name": "Unknown (non-MAVLink or heartbeat-only)",
                "firmware_version": Value::Null,
                "signing_params_present": false,
            })
        );
    }

    #[test]
    fn capability_other_autopilot_is_unsupported_with_a_generic_name() {
        let got = detect_capability(true, 99, false);
        assert_eq!(
            got,
            json!({
                "supported": false,
                "reason": "firmware_not_supported",
                "firmware_name": "Autopilot-99",
                "firmware_version": Value::Null,
                "signing_params_present": false,
            })
        );
    }

    // ── snapshot field extraction ────────────────────────────────────────────

    #[test]
    fn fc_connected_reads_the_snapshot_flag() {
        let snap = json!({ "fc_connected": true });
        assert!(fc_connected_from_snapshot(Some(&snap)));
        let snap = json!({ "fc_connected": false });
        assert!(!fc_connected_from_snapshot(Some(&snap)));
        // Absent snapshot / absent flag both read as disconnected.
        assert!(!fc_connected_from_snapshot(None));
        assert!(!fc_connected_from_snapshot(Some(&json!({}))));
    }

    #[test]
    fn autopilot_reads_the_snapshot_id_and_defaults_to_zero() {
        let snap = json!({ "autopilot": 3 });
        assert_eq!(autopilot_from_snapshot(Some(&snap)), 3);
        // Absent snapshot / absent key / a null value all default to 0.
        assert_eq!(autopilot_from_snapshot(None), 0);
        assert_eq!(autopilot_from_snapshot(Some(&json!({}))), 0);
        assert_eq!(
            autopilot_from_snapshot(Some(&json!({"autopilot": null}))),
            0
        );
    }

    /// A `{name: value}` param map, the shape `crate::param_store` projects.
    fn param_map(body: Value) -> serde_json::Map<String, Value> {
        body.as_object().cloned().unwrap()
    }

    #[test]
    fn signing_params_present_scans_the_param_map() {
        // A SIGNING_* param present.
        assert!(signing_params_present(&param_map(
            json!({ "SIGNING_REQUIRE": 1.0, "FOO": 2.0 })
        )));
        // No SIGNING_* param.
        assert!(!signing_params_present(&param_map(json!({ "FOO": 2.0 }))));
        // An empty map — no cache file, or an FC that has answered nothing yet.
        assert!(!signing_params_present(&serde_json::Map::new()));
        // A name that merely CONTAINS the prefix is not a signing param.
        assert!(!signing_params_present(&param_map(
            json!({ "FOO_SIGNING_X": 1.0 })
        )));
    }

    // ── counters: the golden parity fixture ──────────────────────────────────

    #[tokio::test]
    async fn counters_report_unmeasured_rather_than_zero() {
        // No frame observer exists, so a count of 0 would be a fabricated
        // measurement. The body must say nothing was observed.
        let Json(body) = counters().await;
        assert_eq!(
            body,
            json!({
                "observed": false,
                "tx_signed_count": Value::Null,
                "rx_signed_count": Value::Null,
                "last_signed_rx_at": Value::Null,
            })
        );
    }
}
