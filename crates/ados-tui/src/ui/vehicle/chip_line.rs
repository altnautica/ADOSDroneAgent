//! The Vehicle tab's flight-state chip: the pilot kind aged from its last
//! good read, with the owning skill's detail from its plugin.

use crate::app::Core;
use crate::model::flight_state::{chip_inputs, flight_state_chip, ChipContext};
use ratatui::text::Line;

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

    /// The Pilot block's lines: the chip evaluated on the pilot kind aged
    /// from its last good read, with what the tab knows beside it. The rows
    /// age the same way, so a poll that hangs without failing dims them too.
    pub(super) fn pilot_lines(&self, core: &Core) -> Vec<Line<'static>> {
        let auto = self.auto.as_ref();
        let pilot = auto.and_then(|a| a.pilot.as_ref());
        let follow = auto.and_then(|a| a.follow.as_ref());
        // The FC's own readings beside the pilot kind, while they are shown.
        let tel = self.tel.as_ref().filter(|_| self.hidden(core).is_none());
        let since_read = self.nav_read.ok_at.map_or(0, |at| {
            core.now.saturating_duration_since(at).as_millis() as u64
        });
        // A kind of unknown age is stale, so the chip reads LINK.
        let aged = |age: Option<u64>| age.map_or(u64::MAX, |a| a.saturating_add(since_read));
        let age_ms = pilot.map_or(u64::MAX, |p| aged(p.age_ms));
        let chip = pilot.and_then(|p| {
            let ctx = ChipContext {
                home_dist_m: tel.and_then(|t| t.home_distance),
                local_pos: tel.and_then(|t| t.local_pos),
                eta_s: auto.and_then(|a| a.ar_eta_s),
                mode_detail: self.mode_detail.clone(),
                complete_age_ms: self
                    .complete_at
                    .map(|at| core.now.saturating_duration_since(at).as_millis() as f64),
            };
            chip_inputs(p, age_ms, &ctx).map(|facts| flight_state_chip(&facts))
        });
        pilot::lines(
            core,
            &pilot::PilotInputs {
                pilot,
                age_ms,
                chip: chip.as_ref(),
                follow,
                follow_age_ms: follow.map_or(u64::MAX, |f| aged(f.age_ms)),
                prearm: tel.and_then(|t| t.prearm.as_deref()),
            },
        )
    }
}

/// An 8-point compass word for a heading in degrees.
pub(super) fn compass(deg: f64) -> &'static str {
    const POINTS: [&str; 8] = ["N", "NE", "E", "SE", "S", "SW", "W", "NW"];
    POINTS[((deg.rem_euclid(360.0) + 22.5) / 45.0) as usize % 8]
}
