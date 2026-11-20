//! The fleet address half of `PUT /api/config`: a write to `video.wfb.fleet_id`
//! or `video.wfb.fleet_slot` is judged against the radio's fleet-identity rule
//! and applied as the whole address.

use std::path::Path;

use serde_json::Value;

/// The two keys that make up a node's fleet address.
const FLEET_KEYS: [&str; 2] = ["video.wfb.fleet_id", "video.wfb.fleet_slot"];

/// A write to one half of the fleet address, applied as the whole address.
#[derive(Debug, Clone, Copy)]
pub(super) struct FleetWrite {
    pub(super) identity: ados_supervisor::bind::keys::FleetIdentity,
    pub(super) ground_station: bool,
    /// Whether the radio comes up on the resulting address, so a restart onto
    /// it is worth owing (a half-provisioned node stays parked either way).
    pub(super) usable: bool,
}

/// `(fleet_id, fleet_slot)` of an effective config, as the radio reads them.
fn fleet_address(config: &Value) -> (u16, u8) {
    let id = config
        .pointer("/video/wfb/fleet_id")
        .and_then(Value::as_u64)
        .and_then(|v| u16::try_from(v).ok())
        .unwrap_or(0);
    let slot = config
        .pointer("/video/wfb/fleet_slot")
        .and_then(Value::as_u64)
        .and_then(|v| u8::try_from(v).ok())
        .unwrap_or(u8::MAX);
    (id, slot)
}

/// Judge a write to `key` from `current` to `candidate` against the radio's
/// fleet-identity rule. `Ok(None)` for any other key; `Err` names why the radio
/// would refuse the half being written (a reserved fleet id, slot 0 or a slot
/// past the fleet's ceiling on a drone, a drone slot on a ground station). Only
/// the written half is judged, so a node can still be provisioned one key at a
/// time, and an unchanged value is never refused.
///
/// This is where a slot the ground station's registry issued lands on the
/// drone (its slot reconciler writes `video.wfb.fleet_slot` here), so it is
/// written the way the bind writes one: both halves, together, under the
/// config lock, and the radio restart it owes is recorded.
pub(super) fn fleet_write(
    config_path: &Path,
    key: &str,
    current: &Value,
    candidate: &Value,
) -> Result<Option<FleetWrite>, String> {
    use ados_radio::config::{fleet_identity_error, FIRST_DRONE_SLOT, SLOT_GROUND};
    if !FLEET_KEYS.contains(&key) {
        return Ok(None);
    }
    let ground_station = ados_radio::config::profile_is_ground_station(
        config_path,
        Path::new(ados_radio::config::PROFILE_CONF),
    );
    let (fleet_id, fleet_slot) = fleet_address(candidate);
    if fleet_address(current) != (fleet_id, fleet_slot) {
        // The other half replaced by one the radio accepts, so only this
        // write's own value can be the reason for a refusal.
        let good_slot = if ground_station {
            SLOT_GROUND
        } else {
            FIRST_DRONE_SLOT
        };
        let refusal = if key == FLEET_KEYS[0] {
            fleet_identity_error(fleet_id, good_slot, ground_station)
        } else {
            fleet_identity_error(1, fleet_slot, ground_station)
        };
        if let Some(e) = refusal {
            return Err(e.to_string());
        }
    }
    Ok(Some(FleetWrite {
        identity: ados_supervisor::bind::keys::FleetIdentity {
            fleet_id,
            fleet_slot,
        },
        ground_station,
        usable: fleet_identity_error(fleet_id, fleet_slot, ground_station).is_none(),
    }))
}
