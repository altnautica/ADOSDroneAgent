//! MAVLink v2 signing capability + state read routes.
//!
//! The agent never holds a signing key. These read routes let the GCS detect
//! whether the connected FC supports MAVLink v2 signing, read the current
//! `SIGNING_REQUIRE` param, and read the observational signed-frame counters.
//! All three read from the same vehicle-state snapshot the MAVLink service
//! publishes on `/run/ados/state.sock` (held by the [`StateIpcClient`](crate::ipc)):
//!
//! - **`/api/mavlink/signing/capability`** runs the strict capability check over
//!   the FC connection flag, the autopilot id, and the cached param tree, all
//!   from the snapshot. The check is intentionally strict: ArduPilot + at least
//!   one `SIGNING_*` param present. Any other firmware, or a missing param tree,
//!   reports unsupported with a specific reason enum.
//! - **`/api/mavlink/signing/require`** reads `SIGNING_REQUIRE` from the cached
//!   param blob. `{require: bool}` when the param has been seen, `{require: null}`
//!   when it has not.
//! - **`/api/mavlink/signing/counters`** reports signed-frame counters. The agent
//!   validates nothing (it holds no key); the counters only confirm signed frames
//!   are transiting. In the multi-process supervisor the REST surface has no
//!   in-process frame observer, so the counters are the zero default — the same
//!   shape the FastAPI route emits when its observer is absent.
//!
//! With no agent running (an empty snapshot), capability reports
//! `fc_not_connected`, require reports `{require: null}`, and counters report the
//! zero default — each a valid, GCS-parseable body rather than a failure.

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
/// Reads the FC connection flag, the autopilot id, and the cached param tree from
/// the live state snapshot, then runs the strict capability check. Returns
/// `{supported, reason, firmware_name, firmware_version, signing_params_present}`.
/// The `reason` enum: `ok | fc_not_connected | firmware_not_supported |
/// firmware_too_old | firmware_px4_no_persistent_store | msp_protocol`. An absent
/// snapshot reads as disconnected → `fc_not_connected`. Guaranteed-200, never 500.
pub async fn capability(State(state): State<AppState>) -> Json<Value> {
    let snapshot = state.state.snapshot();
    let connected = fc_connected_from_snapshot(snapshot.as_ref());
    let autopilot = autopilot_from_snapshot(snapshot.as_ref());
    let signing_params_present = signing_params_present_from_snapshot(snapshot.as_ref());
    Json(detect_capability(
        connected,
        autopilot,
        signing_params_present,
    ))
}

/// `GET /api/mavlink/signing/require` → the current `SIGNING_REQUIRE` param value
/// from the cached param blob.
///
/// Returns `{require: bool}` when the param has been seen in the current session,
/// or `{require: null}` when it has not. The boolean is `value != 0` (the Python
/// `bool(int(value))`). An absent snapshot or an absent param both read as
/// `{require: null}`. Guaranteed-200, never 500.
pub async fn require(State(state): State<AppState>) -> Json<Value> {
    let snapshot = state.state.snapshot();
    let value = signing_require_param(snapshot.as_ref());
    Json(get_require(value))
}

/// `GET /api/mavlink/signing/counters` → the observational signed-frame counters.
///
/// The agent holds no signing key and validates nothing; these counters only
/// confirm signed frames are transiting. The multi-process supervisor runs the
/// REST surface as its own service, which carries no in-process frame observer,
/// so the counters are the zero default: `{tx_signed_count: 0, rx_signed_count:
/// 0, last_signed_rx_at: null}`. This is the exact shape the FastAPI route emits
/// when its observer is `None`, which is the production posture.
pub async fn counters() -> Json<Value> {
    Json(json!({
        "tx_signed_count": 0,
        "rx_signed_count": 0,
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

/// Whether any `SIGNING_*` param is present in the snapshot's cached param blob.
/// The blob is a dict (`{PARAM_NAME: value}`); a non-dict or absent blob reads as
/// an empty map. Mirrors the Python `_cached_params` (returns the `params` dict or
/// `{}`) feeding `detect_capability`'s `SIGNING_*` scan.
fn signing_params_present_from_snapshot(snapshot: Option<&Value>) -> bool {
    cached_params(snapshot)
        .map(|params| params.keys().any(|name| name.starts_with("SIGNING_")))
        .unwrap_or(false)
}

/// The cached param blob from a snapshot, as a JSON object, or `None` when the
/// snapshot is absent / the `params` key is absent / it is not an object. Mirrors
/// the Python `_cached_params` returning the dict or `{}`.
fn cached_params(snapshot: Option<&Value>) -> Option<&serde_json::Map<String, Value>> {
    snapshot
        .and_then(Value::as_object)
        .and_then(|m| m.get("params"))
        .and_then(Value::as_object)
}

/// The raw `SIGNING_REQUIRE` param value from a snapshot's cached param blob, or
/// `None` when absent. Mirrors the Python `(params or {}).get("SIGNING_REQUIRE")`.
fn signing_require_param(snapshot: Option<&Value>) -> Option<&Value> {
    cached_params(snapshot).and_then(|params| params.get("SIGNING_REQUIRE"))
}

/// Build the require-read body from the raw param value, mirroring the Python
/// `get_require`. `None` → `{require: null}` (the param has not been seen yet);
/// a present value → `{require: bool}` where the boolean is `int(value) != 0`
/// (the Python `bool(int(value))`). A non-numeric value reads as `null` (the
/// Python `int(value)` would raise; the snapshot only ever carries numeric
/// params, so this is the same defensive shape the route degrades to).
fn get_require(value: Option<&Value>) -> Value {
    match value {
        None => json!({ "require": Value::Null }),
        Some(v) => match v.as_f64() {
            Some(n) => json!({ "require": (n as i64) != 0 }),
            None => json!({ "require": Value::Null }),
        },
    }
}

/// The strict capability check, mirroring the Python `detect_capability`. The FC
/// must be connected, the autopilot must be ArduPilot, and at least one
/// `SIGNING_*` param must be present in the cache. Any other firmware, or a
/// missing param tree, reports unsupported with a specific reason enum.
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
            "signing_params_present": false,
        });
    }

    // ArduPilot. The presence of any SIGNING_* param is the strictest gate: a
    // build that stripped signing won't expose these params. The agent can't
    // derive major/minor from the autopilot id alone, so an absent param tree is
    // treated as too-old (or signing-stripped).
    if !signing_params_present {
        return json!({
            "supported": false,
            "reason": "firmware_too_old",
            "firmware_name": firmware_name,
            "firmware_version": Value::Null,
            "signing_params_present": false,
        });
    }

    json!({
        "supported": true,
        "reason": "ok",
        "firmware_name": firmware_name,
        // The GCS populates firmware_version when it reads AUTOPILOT_VERSION.
        "firmware_version": Value::Null,
        "signing_params_present": true,
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
    fn capability_ardupilot_with_signing_params_is_supported() {
        // The golden "supported" body: ArduPilot, a SIGNING_* param present.
        let got = detect_capability(true, MAV_AUTOPILOT_ARDUPILOTMEGA, true);
        assert_eq!(
            got,
            json!({
                "supported": true,
                "reason": "ok",
                "firmware_name": "ArduPilot",
                "firmware_version": Value::Null,
                "signing_params_present": true,
            })
        );
    }

    #[test]
    fn capability_ardupilot_without_signing_params_is_too_old() {
        let got = detect_capability(true, MAV_AUTOPILOT_ARDUPILOTMEGA, false);
        assert_eq!(
            got,
            json!({
                "supported": false,
                "reason": "firmware_too_old",
                "firmware_name": "ArduPilot",
                "firmware_version": Value::Null,
                "signing_params_present": false,
            })
        );
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
                "signing_params_present": false,
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

    // ── require: the golden parity fixtures ──────────────────────────────────

    #[test]
    fn require_absent_param_is_null() {
        // The golden body when SIGNING_REQUIRE has not been seen.
        assert_eq!(get_require(None), json!({ "require": Value::Null }));
    }

    #[test]
    fn require_zero_is_false() {
        assert_eq!(get_require(Some(&json!(0.0))), json!({ "require": false }));
        assert_eq!(get_require(Some(&json!(0))), json!({ "require": false }));
    }

    #[test]
    fn require_nonzero_is_true() {
        assert_eq!(get_require(Some(&json!(1.0))), json!({ "require": true }));
        assert_eq!(get_require(Some(&json!(1))), json!({ "require": true }));
    }

    #[test]
    fn require_non_numeric_value_degrades_to_null() {
        // A non-numeric param value can never occur in a real snapshot; the route
        // degrades it to null rather than failing.
        assert_eq!(
            get_require(Some(&json!("not a number"))),
            json!({ "require": Value::Null })
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

    #[test]
    fn signing_params_present_scans_the_param_blob() {
        // A SIGNING_* param present.
        let snap = json!({ "params": { "SIGNING_REQUIRE": 1.0, "FOO": 2.0 } });
        assert!(signing_params_present_from_snapshot(Some(&snap)));
        // No SIGNING_* param.
        let snap = json!({ "params": { "FOO": 2.0 } });
        assert!(!signing_params_present_from_snapshot(Some(&snap)));
        // Absent / non-dict params read as empty.
        assert!(!signing_params_present_from_snapshot(Some(&json!({}))));
        assert!(!signing_params_present_from_snapshot(Some(
            &json!({"params": "not a dict"})
        )));
        assert!(!signing_params_present_from_snapshot(None));
    }

    #[test]
    fn signing_require_param_reads_the_blob() {
        let snap = json!({ "params": { "SIGNING_REQUIRE": 1.0 } });
        assert_eq!(signing_require_param(Some(&snap)), Some(&json!(1.0)));
        // Absent param / absent blob both read as None.
        assert_eq!(signing_require_param(Some(&json!({"params": {}}))), None);
        assert_eq!(signing_require_param(None), None);
    }

    // ── counters: the golden parity fixture ──────────────────────────────────

    #[tokio::test]
    async fn counters_is_the_zero_default() {
        // The golden body: the production multi-process posture has no in-process
        // observer, so the counters are the zero default.
        let Json(body) = counters().await;
        assert_eq!(
            body,
            json!({
                "tx_signed_count": 0,
                "rx_signed_count": 0,
                "last_signed_rx_at": Value::Null,
            })
        );
    }
}
