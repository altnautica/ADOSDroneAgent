//! The Vehicle tab's flight-state chip: the pilot kind aged from its last
//! good read, with the owning skill's detail from its plugin; and the status
//! pill's alerts from everything the tab reads.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::app::Core;
use crate::model::autonomy::MissionStall;
use crate::model::base_alerts::{base_alerts, BaseInputs};
use crate::model::flight_state::{chip_inputs, flight_state_chip, stale_after_ms, ChipContext};
use crate::model::nav_source;
use crate::model::pilot_alerts::{pill_alerts, PillAlert, PillInputs};

use super::{pilot, VehicleTab};

impl VehicleTab {
    /// The plugin and skill ids of the skill that owns the aircraft now.
    pub(super) fn owner_skill(&self) -> Option<&(String, String)> {
        let p = self.auto.as_ref()?.pilot.as_ref()?;
        if p.owner.as_deref() != Some("skill") {
            return None;
        }
        p.skill_ref.as_ref()
    }

    /// The pilot kind's age now: its age at the agent plus the time since
    /// the last good read; `u64::MAX` when unknown, so stale.
    fn aged(&self, core: &Core, age: Option<u64>) -> u64 {
        let since_read = self.nav_read.ok_at.map_or(0, |at| {
            core.now.saturating_duration_since(at).as_millis() as u64
        });
        age.map_or(u64::MAX, |a| a.saturating_add(since_read))
    }

    /// The mission stall of a fresh pilot kind: the one the stall keys
    /// answer. `None` with no stall, or while the pilot kind is stale.
    pub(super) fn fresh_stall(&self, core: &Core) -> Option<&MissionStall> {
        let p = self.auto.as_ref()?.pilot.as_ref()?;
        let fresh = !p.stale && self.aged(core, p.age_ms) < stale_after_ms();
        fresh.then_some(p.mission_stall.as_ref()).flatten()
    }

    /// The place class of a fresh pilot kind that reports itself disarmed:
    /// the one the place key switches from. `None` while armed, while the
    /// arm state is unknown (it may be airborne), or while the kind is stale.
    pub(super) fn place_offered(&self, core: &Core) -> Option<&str> {
        let p = self.auto.as_ref()?.pilot.as_ref()?;
        let fresh = !p.stale && self.aged(core, p.age_ms) < stale_after_ms();
        (fresh && p.armed == Some(false)).then_some(p.place_class.as_str())
    }

    /// "I rebalanced it" answers now: a fresh pilot kind that reports itself
    /// disarmed with its airframe balance check failing (a warning or a
    /// block).
    pub(super) fn airframe_reset_offered(&self, core: &Core) -> bool {
        let Some(p) = self.auto.as_ref().and_then(|a| a.pilot.as_ref()) else {
            return false;
        };
        let fresh = !p.stale && self.aged(core, p.age_ms) < stale_after_ms();
        fresh
            && p.armed == Some(false)
            && p.readiness
                .iter()
                .any(|r| r.id == "airframe_balance" && !r.ok)
    }

    /// The status pill's alerts, from the readings the tab holds. Readings
    /// hidden as stale feed nothing but the link and agent items.
    pub(super) fn alerts(&self, core: &Core) -> Vec<PillAlert> {
        let tel = self.tel.as_ref().filter(|_| self.hidden(core).is_none());
        let auto = self.auto.as_ref();
        let pilot = auto.and_then(|a| a.pilot.as_ref());
        let base = base_alerts(&BaseInputs {
            tel,
            battery: self.bat.as_ref(),
            ms_since_live: self
                .live_at
                .map(|at| core.now.saturating_duration_since(at).as_millis() as u64),
            armed_at_last_live: self.armed_at_live,
            fleet_apply: core.dash().and_then(|d| d.fleet_apply.as_ref()),
        });
        // The arm state: the pilot's when it publishes, else the FC's.
        let armed = pilot.map_or(tel.and_then(|t| t.armed), |p| p.armed);
        pill_alerts(&PillInputs {
            alerts: &base,
            pilot,
            pilot_age_ms: pilot.map(|p| self.aged(core, p.age_ms)),
            prearm: tel.and_then(|t| t.prearm.as_deref()),
            storage_pct: core.dash().and_then(|d| d.storage_pct),
            agent_unreachable: core.stale(),
            arm_unknown: armed.is_none(),
            preflight_failing: &[],
            nav_source: Some(nav_source::resolve(
                tel,
                auto.and_then(|a| a.nav.as_ref()),
                pilot
                    .filter(|p| !p.stale)
                    .and_then(|p| p.position_unverified.as_ref()),
            )),
        })
    }

    /// The Pilot block's rows: the chip evaluated on the pilot kind aged
    /// from its last good read, with what the tab knows beside it. The rows
    /// age the same way, so a poll that hangs without failing dims them too.
    pub(super) fn pilot_lines(&self, core: &Core) -> Vec<pilot::Row> {
        let auto = self.auto.as_ref();
        let pilot = auto.and_then(|a| a.pilot.as_ref());
        let follow = auto.and_then(|a| a.follow.as_ref());
        // The FC's own readings beside the pilot kind, while they are shown.
        let tel = self.tel.as_ref().filter(|_| self.hidden(core).is_none());
        let age_ms = pilot.map_or(u64::MAX, |p| self.aged(core, p.age_ms));
        let chip = pilot.and_then(|p| {
            let ctx = ChipContext {
                home_dist_m: tel.and_then(|t| t.home_distance),
                local_pos: tel.and_then(|t| t.local_pos),
                eta_s: auto.and_then(|a| a.ar_eta_s),
                mode_detail: self.mode_detail.clone(),
                complete_age_ms: self
                    .complete_at
                    .map(|at| core.now.saturating_duration_since(at).as_millis() as f64),
                // The Pilot block's Stick row shows the speeds the pilot flies.
                h_speed_mps: None,
                v_speed_mps: None,
            };
            chip_inputs(p, age_ms, &ctx).map(|facts| flight_state_chip(&facts))
        });
        let alerts = self.alerts(core);
        pilot::lines(
            core,
            &pilot::PilotInputs {
                pilot,
                age_ms,
                chip: chip.as_ref(),
                follow,
                follow_age_ms: follow.map_or(u64::MAX, |f| self.aged(core, f.age_ms)),
                alerts: &alerts,
                gimbal: self.gimbal,
                place_key: self.place_offered(core).is_some(),
                airframe_key: self.airframe_reset_offered(core),
                now_ms: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_millis() as u64),
            },
        )
    }

    /// The Pilot block's first line (the flight-state chip) and its alert
    /// count, for the Overview: what the aircraft is doing next to the
    /// agent's health.
    pub(crate) fn summary(&self, core: &Core) -> Vec<ratatui::text::Line<'static>> {
        let state = self
            .pilot_lines(core)
            .into_iter()
            .next()
            .map(|(line, _)| line);
        let count = crate::model::pilot_alerts::alerts_count_text(self.alerts(core).len())
            .unwrap_or_else(|| "–".into());
        let alerts =
            crate::ui::widgets::kv_text(&core.theme, "Alerts", super::autonomy::LABEL, count);
        state.into_iter().chain([alerts]).collect()
    }
}

/// An 8-point compass word for a heading in degrees.
pub(super) fn compass(deg: f64) -> &'static str {
    const POINTS: [&str; 8] = ["N", "NE", "E", "SE", "S", "SW", "W", "NW"];
    POINTS[((deg.rem_euclid(360.0) + 22.5) / 45.0) as usize % 8]
}
