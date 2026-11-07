//! `mission.read`: the flight controller's main mission as the MAVLink router's
//! mission mirror holds it.
//!
//! The router rewrites `mission-mirror.json` beside its state socket whenever
//! the mirror changes (a ground-station download or upload it saw, or its own
//! download). The result is that file's object: `{rev, present, count,
//! home_slot, items: [{param1, param2, param3, param4, x, y, z, seq, command,
//! frame, current, autocontinue, mission_type}]}`, `x`/`y` in 1e-7 degrees.
//! Before anything is mirrored `present` is false and `items` empty. The live
//! mission state (active, paused, …) is in the `telemetry.subscribe` snapshot's
//! `mission` block.

use super::*;

/// The router's mirror file, next to the state socket.
pub(super) const MISSION_MIRROR_FILE: &str = "mission-mirror.json";

impl RealHost {
    pub(super) fn serve_mission_read(&self, _plugin_id: &str) -> Result<HostResult, HostError> {
        let path = self
            .vehicle_state_sock
            .parent()
            .map(|d| d.join(MISSION_MIRROR_FILE))
            .unwrap_or_else(|| PathBuf::from(MISSION_MIRROR_FILE));
        let body = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .filter(serde_json::Value::is_object)
            .unwrap_or_else(|| {
                serde_json::json!({
                    "rev": 0, "present": false, "count": 0, "home_slot": null, "items": [],
                })
            });
        Ok(json_to_mpv(&body))
    }
}
