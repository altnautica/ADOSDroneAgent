//! A fleet slot delivered to this drone and the radio restart it is owed
//! (`radioFleetApply` on `/api/status/full`), as
//! `web-shared/src/hud/status-pill.ts` `fleetSlotAlert` words it: an advisory
//! while the slot waits for the vehicle to land, a warning while the radio
//! restart onto it fails.

use serde_json::Value;

use super::base_alerts::{BaseAlert, Level};
use super::spec_text::spec_text;
use super::Dashboard;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FleetApply {
    /// `pending`, `deferred_armed`, `applied` or `restart_failed`.
    pub outcome: Option<String>,
    pub fleet_slot: Option<u64>,
}

impl FleetApply {
    /// The full status's `radioFleetApply`; `None` when absent or not an
    /// object.
    pub fn from_full(full: &Value) -> Option<Self> {
        let v = full.get("radioFleetApply").filter(|v| v.is_object())?;
        Some(Self {
            outcome: v.get("outcome").and_then(Value::as_str).map(String::from),
            fleet_slot: v.get("fleetSlot").and_then(Value::as_u64),
        })
    }

    /// `fleet_slot_pending` (advisory) or `fleet_slot_failed` (warning);
    /// `None` when nothing is owed or there is no slot to name.
    pub fn alert(&self) -> Option<BaseAlert> {
        let slot = self.fleet_slot?.to_string();
        let (id, level, key) = match self.outcome.as_deref()? {
            "deferred_armed" => (
                "fleet_slot_pending",
                Level::Advisory,
                "alert.fleet_slot_pending",
            ),
            "restart_failed" => (
                "fleet_slot_failed",
                Level::Warning,
                "alert.fleet_slot_failed",
            ),
            _ => return None,
        };
        Some(BaseAlert {
            id: id.to_string(),
            level,
            title: spec_text(key, &[("slot", &slot)]),
        })
    }
}

impl Dashboard {
    /// Merge the fleet slot restart from `/api/status/full`.
    pub fn merge_fleet_apply(&mut self, full: &Value) {
        self.fleet_apply = FleetApply::from_full(full);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fleet(outcome: &str) -> FleetApply {
        FleetApply::from_full(&json!({
            "radioFleetApply": {"outcome": outcome, "fleetId": 7, "fleetSlot": 3, "unit": "ados-wfb-drone"}
        }))
        .unwrap()
    }

    #[test]
    fn a_slot_waiting_for_landing_and_a_failed_restart_are_worded_by_the_spec() {
        let pending = fleet("deferred_armed").alert().unwrap();
        assert_eq!(pending.id, "fleet_slot_pending");
        assert_eq!(pending.level, Level::Advisory);
        assert_eq!(pending.title, "New fleet slot 3 applies after landing.");
        let failed = fleet("restart_failed").alert().unwrap();
        assert_eq!(failed.level, Level::Warning);
        assert_eq!(
            failed.title,
            "Radio restart onto fleet slot 3 failed. Retrying."
        );
        assert_eq!(fleet("applied").alert(), None);
        assert_eq!(fleet("pending").alert(), None);
        assert_eq!(FleetApply::from_full(&json!({})), None);
    }
}
