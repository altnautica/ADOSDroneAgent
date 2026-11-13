//! Touchdown: the pilot's own landed state and the landing descent profile.
//!
//! The FC's `landed_state` turns ON_GROUND only after its land detector has
//! seen a second or so of idle thrust on the ground. Until then the vehicle
//! sits on the ground reported as in the air, and a GUIDED hold in that
//! window cancels the FC's land: its detector never finishes, and a goto
//! then flies off the ground (a SITL copter tipped over doing so). So once
//! there is positive evidence of the ground, the pilot treats the vehicle as
//! landed until a deliberate take-off, a climb or a disarm, and refuses the
//! actions that would command a climb or a hold from there. The evidence is
//! the FC's own ON_GROUND, or, after the pilot put the FC in its land mode, a
//! fresh in-range downward rangefinder reading at the floor with the vehicle
//! still for [`TOUCHDOWN_HOLD_MS`] (the touchdown). Height above home is
//! never evidence: the ground may lie below the take-off point. Safety
//! actions (RTH, Land, Take control, kill) are never refused on this account,
//! and the published `landed` stays the FC's.

use super::control::{BRAKE_LAG_S, LAND_SWITCH_AGL_M};
use super::Pilot;
use crate::flows::HoldDetector;
use ados_protocol::pilot::Owner;

/// A downward rangefinder at or below this reads the floor (it reads
/// 0.1-0.3 m with the vehicle on the ground).
pub const TOUCHDOWN_AGL_M: f32 = 0.3;
/// ... with a vertical speed under this.
pub const TOUCHDOWN_VZ_MPS: f32 = 0.2;
/// ... both held this long.
pub const TOUCHDOWN_HOLD_MS: u64 = 200;
/// A climb this far above the lowest point of a touchdown ends it.
pub const TOUCHDOWN_CLEAR_M: f32 = 1.0;
/// The FC's landing speed when it reports none (ArduCopter's default).
pub const LAND_SPEED_DEFAULT_MPS: f32 = 0.5;
/// The pilot's landing descent slows at this rate toward the hand-over.
pub const LAND_DECEL_MPS2: f32 = 1.0;

/// The pilot's view of a landing reaching the ground.
#[derive(Debug, Clone, Default)]
pub struct Touchdown {
    /// On the ground: the NED down of the floor (the lowest point seen).
    pub floor_d: Option<f32>,
    /// The pilot put the FC in its land mode and it has not yet touched down.
    pub handed_over: bool,
    /// The rangefinder floor reading held.
    pub hold: HoldDetector,
}

impl Pilot {
    /// On the ground as the pilot sees it: the FC says so, or a touchdown
    /// holds while armed.
    pub fn landed(&self) -> bool {
        self.fc.landed() || self.fc.armed && self.touchdown.floor_d.is_some()
    }

    /// The pilot has just put the FC in its land mode.
    pub(super) fn land_handed_over(&mut self) {
        self.touchdown.handed_over = true;
    }

    /// Every tick: start a touchdown on ground evidence, track the floor, and
    /// end it on a climb of [`TOUCHDOWN_CLEAR_M`].
    pub(super) fn track_touchdown(&mut self, now_ms: u64) {
        if !self.fc.armed {
            self.touchdown = Touchdown::default();
            return;
        }
        let Some(p) = self.fc.fresh_local(now_ms) else {
            return;
        };
        if let Some(floor) = self.touchdown.floor_d {
            let floor = floor.max(p.d);
            if p.d > floor - TOUCHDOWN_CLEAR_M {
                self.touchdown.floor_d = Some(floor);
            } else {
                self.touchdown = Touchdown::default();
            }
            return;
        }
        // Someone else flies now (Take control, RTH): the hand-over is over.
        if self.owner() != Owner::FcMode {
            self.touchdown.handed_over = false;
        }
        let at_floor = self.touchdown.handed_over
            && self
                .fc
                .rangefinder_agl(now_ms)
                .is_some_and(|a| a <= TOUCHDOWN_AGL_M)
            && p.vd.abs() <= TOUCHDOWN_VZ_MPS;
        let floor_held = self
            .touchdown
            .hold
            .update(at_floor, now_ms, TOUCHDOWN_HOLD_MS);
        if floor_held {
            self.touchdown = Touchdown {
                floor_d: Some(p.d),
                ..Touchdown::default()
            };
        }
    }

    /// Actions a touchdown refuses: those that would command a climb or a
    /// hold off the ground (`landed`); a take-off waits until the FC itself
    /// reports the vehicle on the ground (`bad_state`, "not available at this
    /// point").
    pub(super) fn touchdown_refusal(&self, name: &str) -> Result<(), String> {
        if self.touchdown.floor_d.is_none() || !self.fc.armed {
            return Ok(());
        }
        match name.split(':').next() {
            Some("pause" | "goto" | "resume" | "vtol") => Err("landed".into()),
            Some("takeoff" | "launch") if !self.fc.landed() => Err("bad_state".into()),
            _ => Ok(()),
        }
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
