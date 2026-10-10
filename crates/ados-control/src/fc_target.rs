//! Which MAVLink `(system, component)` this agent addresses its flight
//! controller as.
//!
//! The fleet gives each aircraft its own system id, so the MAVLink convention
//! of "the autopilot is 1/1" is wrong on every drone but one. Every surface that
//! sends a COMMAND_LONG, PARAM_SET or signing setup to the vehicle resolves its
//! target here, once per request, so they all agree.
//!
//! Resolution order:
//! 1. `fc_system_id` / `fc_component_id` from a fresh router snapshot (the ids
//!    latched from the autopilot's own HEARTBEAT), within
//!    [`ARMED_STATE_MAX_AGE`];
//! 2. the cached `SYSID_THISMAV` parameter, with component 1;
//! 3. `(1, 1)`.

use ados_protocol::armed::ARMED_STATE_MAX_AGE;
use serde_json::Value;

use crate::state::AppState;

/// The target used when nothing better is known.
pub const DEFAULT_TARGET: (u8, u8) = (1, 1);

/// The autopilot's own system-id parameter, by firmware generation:
/// ArduPilot before 4.7 names it `SYSID_THISMAV`, 4.7 and later `MAV_SYSID`.
pub const SYSID_PARAMS: [&str; 2] = ["SYSID_THISMAV", "MAV_SYSID"];

/// The system-id parameter the FC's param cache holds, with its cached value.
/// When the cache holds neither, the pre-4.7 name is returned with no value.
pub fn cached_sysid_param(params_path: &std::path::Path) -> (&'static str, Option<f64>) {
    for name in SYSID_PARAMS {
        if let Some(p) = crate::param_store::read_param(params_path, name)
            .ok()
            .flatten()
        {
            return (name, Some(p.value));
        }
    }
    (SYSID_PARAMS[0], None)
}

/// Resolve the flight controller's `(system, component)` for one request.
pub fn fc_target(state: &AppState) -> (u8, u8) {
    let snapshot = state
        .state
        .snapshot_at()
        .filter(|(at, _)| at.elapsed() <= ARMED_STATE_MAX_AGE)
        .map(|(_, v)| v);
    let (_, cached_sysid) = cached_sysid_param(&state.params_path);
    resolve(snapshot.as_ref(), cached_sysid)
}

/// The pure precedence over a fresh snapshot and a cached `SYSID_THISMAV`.
fn resolve(snapshot: Option<&Value>, cached_sysid: Option<f64>) -> (u8, u8) {
    if let Some(snap) = snapshot {
        let id = |key: &str| {
            snap.get(key)
                .and_then(Value::as_u64)
                .and_then(|v| u8::try_from(v).ok())
        };
        if let (Some(system), Some(component)) = (id("fc_system_id"), id("fc_component_id")) {
            if system != 0 {
                return (system, component);
            }
        }
    }
    if let Some(v) = cached_sysid {
        if v.fract() == 0.0 && (1.0..=255.0).contains(&v) {
            return (v as u8, 1);
        }
    }
    DEFAULT_TARGET
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn precedence_table() {
        let latched = json!({"fc_system_id": 7, "fc_component_id": 1});
        let unlatched = json!({"armed": false});
        type Case<'a> = (Option<&'a Value>, Option<f64>, (u8, u8));
        let cases: [Case; 8] = [
            // A fresh latched snapshot wins over everything.
            (Some(&latched), Some(3.0), (7, 1)),
            (Some(&latched), None, (7, 1)),
            // No latch yet: the cached parameter, component 1.
            (Some(&unlatched), Some(3.0), (3, 1)),
            (None, Some(42.0), (42, 1)),
            // An out-of-range or fractional parameter is ignored.
            (None, Some(0.0), DEFAULT_TARGET),
            (None, Some(256.0), DEFAULT_TARGET),
            (None, Some(2.5), DEFAULT_TARGET),
            // Nothing known.
            (None, None, DEFAULT_TARGET),
        ];
        for (snap, sysid, want) in cases {
            assert_eq!(resolve(snap, sysid), want, "{snap:?} / {sysid:?}");
        }
    }

    #[test]
    fn out_of_range_snapshot_ids_fall_through() {
        let bad = json!({"fc_system_id": 300, "fc_component_id": 1});
        assert_eq!(resolve(Some(&bad), Some(5.0)), (5, 1));
        let zero = json!({"fc_system_id": 0, "fc_component_id": 1});
        assert_eq!(resolve(Some(&zero), None), DEFAULT_TARGET);
    }

    #[test]
    fn cached_sysid_accepts_both_firmware_spellings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("params.json");
        for name in ["SYSID_THISMAV", "MAV_SYSID"] {
            std::fs::write(&path, json!({ name: { "value": 9 } }).to_string()).unwrap();
            assert_eq!(cached_sysid_param(&path), (name, Some(9.0)));
        }
        std::fs::write(&path, json!({ "OTHER": { "value": 1 } }).to_string()).unwrap();
        assert_eq!(cached_sysid_param(&path), ("SYSID_THISMAV", None));
    }
}
