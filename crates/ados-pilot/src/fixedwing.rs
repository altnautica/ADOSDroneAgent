//! Fixed-wing stick model (C2b): roll → heading rate, pitch → climb rate,
//! throttle → airspeed, integrated into heading/altitude/airspeed targets that
//! go to a Plane in GUIDED as `MAV_CMD_GUIDED_CHANGE_*`, at most 10 Hz and
//! only on change. Centred sticks hold heading, altitude and airspeed.

use ados_protocol::mavlink::ardupilotmega::{MavCmd, MavFrame, MavMessage};
use ados_protocol::pilot::Profile;

use crate::fc::Tx;
use crate::model::expo;

/// Minimum spacing between command bursts.
pub const SEND_PERIOD_MS: u64 = 100;

/// Plane limits read from the FC (with ArduPlane defaults).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlaneLimits {
    pub climb_max: f32,
    pub sink_max: f32,
    pub airspeed_min: f32,
    pub airspeed_max: f32,
    pub airspeed_cruise: f32,
}

impl PlaneLimits {
    pub fn from_params(p: impl Fn(&str) -> Option<f32>) -> Self {
        let cruise = p("AIRSPEED_CRUISE")
            .or_else(|| p("TRIM_ARSPD_CM").map(|c| c / 100.0))
            .unwrap_or(15.0);
        Self {
            climb_max: p("TECS_CLMB_MAX").unwrap_or(5.0),
            sink_max: p("TECS_SINK_MAX").unwrap_or(5.0),
            airspeed_min: p("AIRSPEED_MIN")
                .or_else(|| p("ARSPD_FBW_MIN"))
                .unwrap_or(9.0),
            airspeed_max: p("AIRSPEED_MAX")
                .or_else(|| p("ARSPD_FBW_MAX"))
                .unwrap_or(22.0),
            airspeed_cruise: cruise,
        }
    }
}

/// Profile scaling: heading-rate limit (°/s) and climb fraction.
pub fn profile_scale(p: Profile) -> (f32, f32) {
    match p {
        Profile::Cine => (15.0, 0.5),
        Profile::Normal => (30.0, 0.75),
        Profile::Sport => (45.0, 1.0),
    }
}

#[derive(Debug, Clone, Default)]
pub struct Fixedwing {
    /// Target course, degrees [0, 360).
    pub heading_deg: Option<f32>,
    /// Target altitude above home, metres.
    pub alt_m: Option<f32>,
    pub airspeed: Option<f32>,
    sent: Option<(f32, f32, f32)>,
    sent_ms: u64,
}

impl Fixedwing {
    pub fn engage(&mut self, heading_deg: f32, alt_m: f32, cruise: f32) {
        self.heading_deg = Some(heading_deg.rem_euclid(360.0));
        self.alt_m = Some(alt_m);
        self.airspeed = Some(cruise);
        self.sent = None;
    }

    /// One tick: integrate the sticks and return the commands to send.
    #[allow(clippy::too_many_arguments)]
    pub fn step(
        &mut self,
        axes: [f32; 4],
        profile: Profile,
        lim: &PlaneLimits,
        min_alt_m: f32,
        dt: f32,
        now_ms: u64,
        tx: &Tx,
    ) -> Vec<MavMessage> {
        let (rate_dps, climb_frac) = profile_scale(profile);
        let [roll, pitch, thr, _] = axes;
        let e = 0.2;
        let h = self.heading_deg.get_or_insert(0.0);
        *h = (*h + expo(roll, e) * rate_dps * dt).rem_euclid(360.0);
        let climb = expo(pitch, e);
        let rate = if climb >= 0.0 {
            climb * lim.climb_max
        } else {
            climb * lim.sink_max
        } * climb_frac;
        let a = self.alt_m.get_or_insert(min_alt_m);
        *a = (*a + rate * dt).max(min_alt_m);
        let t = expo(thr, e);
        let span = if t >= 0.0 {
            lim.airspeed_max - lim.airspeed_cruise
        } else {
            lim.airspeed_cruise - lim.airspeed_min
        };
        self.airspeed = Some(lim.airspeed_cruise + t * span);
        let now = (
            self.heading_deg.unwrap_or(0.0),
            self.alt_m.unwrap_or(min_alt_m),
            self.airspeed.unwrap_or(0.0),
        );
        if now_ms.saturating_sub(self.sent_ms) < SEND_PERIOD_MS {
            return Vec::new();
        }
        let mut out = Vec::new();
        let prev = self.sent;
        let changed = |p: Option<f32>, v: f32, eps: f32| p.is_none_or(|p| (p - v).abs() > eps);
        if changed(prev.map(|p| p.0), now.0, 0.5) {
            out.push(tx.command_int(
                MavCmd::MAV_CMD_GUIDED_CHANGE_HEADING,
                [0.0, now.0, 2.0, 0.0],
                0.0,
                MavFrame::MAV_FRAME_GLOBAL,
            ));
        }
        if changed(prev.map(|p| p.1), now.1, 0.3) {
            out.push(tx.command_int(
                MavCmd::MAV_CMD_GUIDED_CHANGE_ALTITUDE,
                [0.0, 0.0, rate.abs().max(1.0), 0.0],
                now.1,
                MavFrame::MAV_FRAME_GLOBAL_RELATIVE_ALT,
            ));
        }
        if changed(prev.map(|p| p.2), now.2, 0.3) {
            out.push(tx.command_int(
                MavCmd::MAV_CMD_GUIDED_CHANGE_SPEED,
                [0.0, now.2, 0.0, 0.0],
                0.0,
                MavFrame::MAV_FRAME_GLOBAL,
            ));
        }
        if !out.is_empty() {
            self.sent = Some(now);
            self.sent_ms = now_ms;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lim() -> PlaneLimits {
        PlaneLimits::from_params(|_| None)
    }

    #[test]
    fn centred_sticks_send_once_then_hold() {
        let tx = Tx {
            sysid: 1,
            compid: 1,
        };
        let mut f = Fixedwing::default();
        f.engage(90.0, 80.0, 15.0);
        let first = f.step([0.0; 4], Profile::Normal, &lim(), 30.0, 0.02, 1000, &tx);
        assert_eq!(first.len(), 3);
        let again = f.step([0.0; 4], Profile::Normal, &lim(), 30.0, 0.02, 1200, &tx);
        assert!(again.is_empty(), "nothing changed");
    }

    #[test]
    fn roll_turns_and_altitude_floor_holds() {
        let tx = Tx {
            sysid: 1,
            compid: 1,
        };
        let mut f = Fixedwing::default();
        f.engage(0.0, 31.0, 15.0);
        for k in 0..50 {
            f.step(
                [1.0, -1.0, 0.0, 0.0],
                Profile::Normal,
                &lim(),
                30.0,
                0.02,
                k * 20,
                &tx,
            );
        }
        let h = f.heading_deg.unwrap();
        assert!((h - 30.0).abs() < 0.5, "1 s at 30°/s, got {h}");
        assert_eq!(f.alt_m.unwrap(), 30.0);
    }

    #[test]
    fn rate_limited_to_10_hz() {
        let tx = Tx {
            sysid: 1,
            compid: 1,
        };
        let mut f = Fixedwing::default();
        f.engage(0.0, 80.0, 15.0);
        let mut sends = 0;
        for k in 0..50 {
            if !f
                .step(
                    [1.0, 0.0, 0.0, 0.0],
                    Profile::Sport,
                    &lim(),
                    30.0,
                    0.02,
                    1000 + k * 20,
                    &tx,
                )
                .is_empty()
            {
                sends += 1;
            }
        }
        assert!(sends <= 10, "{sends} bursts in 1 s");
    }
}
