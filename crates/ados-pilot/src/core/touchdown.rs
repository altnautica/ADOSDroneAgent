//! Touchdown: the pilot's own landed state and the landing descent profile.
//!
//! The FC's `landed_state` turns ON_GROUND only after its land detector has
//! seen a second or so of idle thrust on the ground. Until then the vehicle
//! sits on the ground reported as in the air, and a GUIDED switch in that
//! window cancels the FC's land: its detector never finishes, and a goto
//! then flies off the ground (a SITL copter tipped over doing so). So once a
//! landing reaches the floor, or the pilot's own Land has handed over to the
//! FC's land mode, the pilot treats the vehicle as landed until a deliberate
//! take-off, a climb or a disarm, and never takes the FC out of its land.

use super::control::{BRAKE_LAG_S, LAND_SWITCH_AGL_M};
use super::Pilot;

/// A landing is at the floor below this height above ground (a downward
/// rangefinder reads 0.1-0.3 m on the ground).
pub const TOUCHDOWN_AGL_M: f32 = 0.3;
/// ... and with a vertical speed under this.
pub const TOUCHDOWN_VZ_MPS: f32 = 0.2;
/// A climb this far above the lowest point of a touchdown ends it.
pub const TOUCHDOWN_CLEAR_M: f32 = 1.0;
/// The FC's landing speed when it reports none (ArduCopter's default).
pub const LAND_SPEED_DEFAULT_MPS: f32 = 0.5;
/// The pilot's landing descent slows at this rate toward the hand-over.
pub const LAND_DECEL_MPS2: f32 = 1.0;

impl Pilot {
    /// On the ground as the pilot sees it: the FC says so, or a touchdown
    /// holds while armed.
    pub fn landed(&self) -> bool {
        self.fc.landed() || self.fc.armed && self.touchdown.is_some()
    }

    /// Start a touchdown with the vehicle at NED down `d`.
    pub(super) fn touch_down(&mut self, d: f32) {
        self.touchdown = Some(d);
    }

    /// Every tick: notice a landing at the floor, track the floor, and end
    /// the touchdown on a climb of [`TOUCHDOWN_CLEAR_M`].
    pub(super) fn track_touchdown(&mut self, now_ms: u64) {
        if !self.fc.armed {
            self.touchdown = None;
            return;
        }
        let Some(p) = self.fc.fresh_local(now_ms) else {
            return;
        };
        if let Some(floor) = self.touchdown {
            let floor = floor.max(p.d);
            self.touchdown = (p.d > floor - TOUCHDOWN_CLEAR_M).then_some(floor);
            return;
        }
        let fam = self.family();
        let mode = self.fc.custom_mode;
        let descending = self.landing || fam.is_land(mode) || fam.is_rtl(mode);
        let at_floor = self.agl(now_ms).is_some_and(|a| a <= TOUCHDOWN_AGL_M)
            && p.vd.abs() <= TOUCHDOWN_VZ_MPS;
        if descending && at_floor {
            self.touch_down(p.d);
        }
    }

    /// Actions a touchdown refuses: anything that would take the FC out of
    /// its land or fly off the ground (`landed`); a take-off waits until the
    /// FC itself reports the vehicle on the ground (`bad_state`, "not
    /// available at this point").
    pub(super) fn touchdown_refusal(&self, name: &str) -> Result<(), String> {
        if self.touchdown.is_none() || !self.fc.armed {
            return Ok(());
        }
        match name.split(':').next() {
            Some("pause" | "goto" | "take_control" | "rth" | "resume" | "hand_to_fc" | "vtol") => {
                Err("landed".into())
            }
            Some("takeoff" | "launch") if !self.fc.landed() => Err("bad_state".into()),
            _ => Ok(()),
        }
    }

    /// True when `mode` would take the FC out of the land or return it flies
    /// to a touchdown the FC has not yet confirmed.
    pub(super) fn holds_land(&self, mode: u32) -> bool {
        let fam = self.family();
        let current = self.fc.custom_mode;
        let held = self.touchdown.is_some()
            && self.fc.armed
            && !self.fc.landed()
            && (fam.is_land(current) || fam.is_rtl(current))
            && !fam.is_land(mode);
        if held {
            tracing::warn!(
                mode,
                current,
                "mode change held: the FC is finishing a landing"
            );
        }
        held
    }

    /// The FC's landing speed: `LAND_SPD_MS` (m/s), `LAND_SPEED` (cm/s) on
    /// older firmware, else [`LAND_SPEED_DEFAULT_MPS`].
    pub fn land_speed(&self) -> f32 {
        let p = &self.fc.params;
        p.get("LAND_SPD_MS")
            .copied()
            .or_else(|| p.get("LAND_SPEED").map(|cm| cm / 100.0))
            .filter(|v| *v > 0.0)
            .unwrap_or(LAND_SPEED_DEFAULT_MPS)
    }

    /// The fastest descent the pilot's Land flies at `agl` while descending
    /// at `vd`: it slows at [`LAND_DECEL_MPS2`] so that, after the FC's lag,
    /// the hand-over at [`LAND_SWITCH_AGL_M`] comes at the FC's landing speed.
    pub(super) fn land_descent_cap(&self, agl: f32, vd: f32) -> f32 {
        let v_land = self.land_speed();
        let h = (agl - LAND_SWITCH_AGL_M - vd.max(0.0) * BRAKE_LAG_S).max(0.0);
        (v_land * v_land + 2.0 * LAND_DECEL_MPS2 * h).sqrt()
    }
}
