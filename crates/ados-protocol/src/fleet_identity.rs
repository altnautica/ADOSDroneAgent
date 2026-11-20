//! This node's own fleet slot, read from operator config.
//!
//! The slot is the low 8 bits of the wfb `link_id`, so it is how a node is
//! addressed on the shared radio. Several processes need to know their own —
//! the radio to key its transmitters, and the MAVLink router to tell whether a
//! record broadcast to the whole fleet is about this node — and they live in
//! crates that do not depend on one another.
//!
//! Read-only: nothing here issues or negotiates a slot. The ground station
//! allocates slots at pair time and the value arrives in config.

use serde::{Deserialize, Serialize};

/// The ground station's slot. Drones take 1..=N.
pub const SLOT_GROUND: u8 = 0;

#[derive(Debug, Default, Deserialize)]
struct RawRoot {
    #[serde(default)]
    video: RawVideo,
}

#[derive(Debug, Default, Deserialize)]
struct RawVideo {
    #[serde(default)]
    wfb: RawWfb,
}

#[derive(Debug, Default, Deserialize)]
struct RawWfb {
    #[serde(default)]
    fleet_slot: Option<u8>,
}

/// Resolve this node's slot from config text.
///
/// `None` when the config names no slot. That is deliberately distinct from
/// slot 0: a node with no slot has not been provisioned, and a caller deciding
/// whether a fleet-wide broadcast is addressed to it must not conclude "yes,
/// I am the ground station" from an absent value.
pub fn local_slot_from_yaml(text: &str) -> Option<u8> {
    let raw: RawRoot = serde_norway::from_str(text).unwrap_or_default();
    raw.video.wfb.fleet_slot
}

/// Resolve from a config file. `None` when absent, unreadable, or unset.
pub fn local_slot_from(path: &std::path::Path) -> Option<u8> {
    let text = std::fs::read_to_string(path).ok()?;
    local_slot_from_yaml(&text)
}

/// Resolve from the agent's config file.
pub fn local_slot() -> Option<u8> {
    local_slot_from(&crate::aux_ports::config_path())
}

/// The run-directory record of a fleet address written to config that the
/// radio has still to restart onto (ados-control's `radio_fleet_apply` writes
/// it). The radio reads the address only when it comes up, so a delivered slot
/// takes effect on a radio restart, which waits for the vehicle to disarm.
pub const RADIO_FLEET_APPLY_JSON: &str = "radio-fleet-apply.json";

/// The fleet-address restart as the status surfaces serve it (the full status
/// `radioFleetApply`, the cloud heartbeat's `radio.fleet_apply`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FleetApplyView {
    /// `pending` (recorded, not yet judged), `deferred_armed` (waiting for the
    /// vehicle to disarm), `applied`, or `restart_failed` (retried).
    pub outcome: String,
    pub fleet_id: Option<u16>,
    pub fleet_slot: Option<u8>,
    /// Unix milliseconds the address was written.
    pub since_ms: Option<u64>,
    /// Unix milliseconds of the last outcome change.
    pub at_ms: Option<u64>,
}

/// The record at `path`, `None` when absent, unreadable or without an outcome.
pub fn fleet_apply_view_at(path: &std::path::Path) -> Option<FleetApplyView> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// [`fleet_apply_view_at`] on `<run dir>/radio-fleet-apply.json`.
pub fn fleet_apply_view() -> Option<FleetApplyView> {
    fleet_apply_view_at(&crate::paths::run_dir().join(RADIO_FLEET_APPLY_JSON))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_configured_slot_is_read() {
        assert_eq!(
            local_slot_from_yaml("video:\n  wfb:\n    fleet_slot: 3\n"),
            Some(3)
        );
        assert_eq!(
            local_slot_from_yaml("video:\n  wfb:\n    fleet_slot: 0\n"),
            Some(SLOT_GROUND)
        );
    }

    #[test]
    fn an_absent_slot_is_none_not_the_ground_station() {
        // The distinction matters: a caller deciding whether a fleet-wide
        // broadcast is addressed to it must not read "unprovisioned" as "I am
        // slot 0".
        assert_eq!(local_slot_from_yaml(""), None);
        assert_eq!(
            local_slot_from_yaml("video:\n  wfb:\n    channel: 149\n"),
            None
        );
    }

    #[test]
    fn malformed_yaml_is_none_rather_than_a_panic() {
        assert_eq!(local_slot_from_yaml("video: [not a map"), None);
    }

    #[test]
    fn a_missing_file_is_none() {
        assert_eq!(
            local_slot_from(std::path::Path::new("/nonexistent/ados/config.yaml")),
            None
        );
    }

    #[test]
    fn the_fleet_apply_record_reads_as_its_view() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(RADIO_FLEET_APPLY_JSON);
        assert_eq!(fleet_apply_view_at(&path), None);
        std::fs::write(
            &path,
            br#"{"version":1,"outcome":"deferred_armed","fleet_id":7,"fleet_slot":3,"unit":"ados-wfb-drone","since_ms":10,"at_ms":20}"#,
        )
        .unwrap();
        assert_eq!(
            fleet_apply_view_at(&path),
            Some(FleetApplyView {
                outcome: "deferred_armed".into(),
                fleet_id: Some(7),
                fleet_slot: Some(3),
                since_ms: Some(10),
                at_ms: Some(20),
            })
        );
        std::fs::write(&path, b"{}").unwrap();
        assert_eq!(fleet_apply_view_at(&path), None);
    }
}
