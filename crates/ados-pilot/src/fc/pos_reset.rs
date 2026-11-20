//! Horizontal position resets of the FC's EKF: a source-set change, the
//! origin first set in flight, a GPS glitch and its recovery, a lane switch.
//! The estimate steps while the vehicle does not move, and ArduCopter's
//! GUIDED position submodes keep their absolute target through the step
//! (`ModeGuided::move_vehicle_on_ekf_reset`): the FC then flies the size of
//! the step to the target's old place in the new frame.
//!
//! ArduPilot publishes no reset counter over MAVLink, so a reset is seen in
//! LOCAL_POSITION_NED itself: a horizontal step its own velocities do not
//! explain, judged on the FC's `time_boot_ms` so a stalled link or a stepped
//! bench clock never reads as motion. The FC's explicit signals (its EKF and
//! GPS-glitch STATUSTEXT, a GPS_GLOBAL_ORIGIN set or moved while armed, the
//! pilot's own source-set change) name the cause and lower the step that
//! counts for a while: alone they say neither that the frame moved nor how
//! far.

use super::LocalPos;
use crate::geo::{self, V2};

/// An unexplained step this large is a reset without any signal, m.
pub const STEP_M: f32 = 2.0;
/// Within [`CUE_WINDOW_MS`] of an explicit signal, a step this large is, m.
pub const CUED_STEP_M: f32 = 0.5;
/// How long an explicit signal names the cause of a step, ms.
pub const CUE_WINDOW_MS: u64 = 5_000;
/// Acceleration the trapezoid of two velocities may leave unexplained
/// between samples, m/s²: the allowance grows by half of it times dt².
pub const ACCEL_SLACK_MPS2: f32 = 10.0;
/// Samples further apart on the FC's clock are not compared (a link gap, a
/// bench clock stepped forward): the next pair is.
pub const MAX_GAP_MS: u32 = 1_000;

/// What reset the estimate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetCause {
    /// The EKF origin was set or moved while armed.
    OriginSet,
    /// A GPS glitch, or its recovery.
    GpsGlitch,
    /// The EKF changed its primary lane.
    LaneSwitch,
    /// The EKF took up another position source.
    SourceSet,
    /// A step no signal named.
    PositionStep,
}

impl ResetCause {
    /// The stable wire code.
    pub fn code(self) -> &'static str {
        match self {
            Self::OriginSet => "origin_set",
            Self::GpsGlitch => "gps_glitch",
            Self::LaneSwitch => "lane_switch",
            Self::SourceSet => "source_set",
            Self::PositionStep => "position_step",
        }
    }

    /// The cause an FC STATUSTEXT names, if any: ArduPilot's EKF3
    /// (`EKF3 IMU0 origin set`, `EKF3 lane switch 1`, `EKF primary changed`,
    /// `EKF3 IMU0 is using GPS`, `... is using external nav data`,
    /// `... fusing odometry`) and ArduCopter's `GPS Glitch or Compass error`
    /// and `Glitch cleared`.
    pub fn of_text(text: &str) -> Option<Self> {
        if text.contains("origin set") {
            Some(Self::OriginSet)
        } else if text.contains("Glitch") {
            Some(Self::GpsGlitch)
        } else if text.contains("lane switch") || text.contains("primary changed") {
            Some(Self::LaneSwitch)
        } else if text.contains("is using ") || text.contains("fusing odometry") {
            Some(Self::SourceSet)
        } else {
            None
        }
    }
}

/// One reset seen: how far the estimate stepped (north, east, m) beyond
/// what the vehicle flew, and why.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PosReset {
    pub shift: V2,
    pub cause: ResetCause,
    pub at_ms: u64,
}

/// The last sample compared against.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Sample {
    boot_ms: u32,
    ne: V2,
    vel: V2,
}

/// Watches LOCAL_POSITION_NED and the FC's signals for position resets.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResetWatch {
    last: Option<Sample>,
    /// The latest explicit signal while armed, and when.
    cue: Option<(ResetCause, u64)>,
    /// The last GPS_GLOBAL_ORIGIN, lat and lon in 1e-7 degrees.
    origin: Option<(i32, i32)>,
    /// A reset seen and not yet taken by the pilot.
    pending: Option<PosReset>,
}

impl ResetWatch {
    /// An explicit signal while armed: it names the cause of a step within
    /// [`CUE_WINDOW_MS`].
    pub fn cue(&mut self, cause: ResetCause, now_ms: u64) {
        self.cue = Some((cause, now_ms));
    }

    /// An FC STATUSTEXT.
    pub fn on_text(&mut self, text: &str, armed: bool, now_ms: u64) {
        if let Some(c) = ResetCause::of_text(text).filter(|_| armed) {
            self.cue(c, now_ms);
        }
    }

    /// A GPS_GLOBAL_ORIGIN: the origin set while armed, or moved.
    pub fn on_origin(&mut self, lat: i32, lon: i32, armed: bool, now_ms: u64) {
        let moved = self.origin.is_some_and(|o| o != (lat, lon));
        if (armed && self.origin.is_none()) || moved {
            self.cue(ResetCause::OriginSet, now_ms);
        }
        self.origin = Some((lat, lon));
    }

    /// The FC rebooted: nothing before compares with what follows.
    pub fn reboot(&mut self) {
        *self = Self::default();
    }

    /// A LOCAL_POSITION_NED stamped `boot_ms` on the FC's clock. A sample
    /// with no FC time (0) is never judged.
    pub fn on_local(&mut self, boot_ms: u32, p: &LocalPos, now_ms: u64) {
        let cur = Sample {
            boot_ms,
            ne: [p.n, p.e],
            vel: [p.vn, p.ve],
        };
        let Some(prev) = self.last else {
            self.last = (boot_ms != 0).then_some(cur);
            return;
        };
        let rebooted = boot_ms.saturating_add(super::REBOOT_BACKSTEP_MS) < prev.boot_ms;
        if boot_ms != 0 && boot_ms <= prev.boot_ms && !rebooted {
            // The same sample again, or an older one reordered on the link.
            return;
        }
        let dt_ms = boot_ms.wrapping_sub(prev.boot_ms);
        if boot_ms == 0 || rebooted || dt_ms > MAX_GAP_MS {
            // No FC time, a rebooted FC, or too long a gap to compare over
            // (a link outage, a bench clock stepped on).
            self.last = (boot_ms != 0).then_some(cur);
            return;
        }
        let dt = dt_ms as f32 / 1000.0;
        let flown = geo::scale(geo::add(prev.vel, cur.vel), 0.5 * dt);
        let step = geo::sub(geo::sub(cur.ne, prev.ne), flown);
        let cue = self
            .cue
            .filter(|(_, t)| now_ms.saturating_sub(*t) <= CUE_WINDOW_MS);
        let base = if cue.is_some() { CUED_STEP_M } else { STEP_M };
        let allow = base + 0.5 * ACCEL_SLACK_MPS2 * dt * dt;
        if geo::norm(step) > allow {
            self.pending = Some(PosReset {
                shift: step,
                cause: cue.map_or(ResetCause::PositionStep, |(c, _)| c),
                at_ms: now_ms,
            });
        }
        self.last = Some(cur);
    }

    /// The reset seen since the last call, if any.
    pub fn take(&mut self) -> Option<PosReset> {
        self.pending.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(n: f32, e: f32, vn: f32, ve: f32) -> LocalPos {
        LocalPos {
            n,
            e,
            vn,
            ve,
            ..Default::default()
        }
    }

    /// Fly a straight line at `v` m/s north, sampled every `period` ms of
    /// FC time, from `t0` for `n` samples.
    fn fly(w: &mut ResetWatch, t0: u32, period: u32, n: u32, v: f32) {
        for i in 0..n {
            let t = t0 + i * period;
            let x = v * (t - t0) as f32 / 1000.0;
            w.on_local(t, &local(x, 0.0, v, 0.0), u64::from(t));
        }
    }

    #[test]
    fn a_21_m_step_in_a_hover_is_a_reset() {
        let mut w = ResetWatch::default();
        fly(&mut w, 1_000, 100, 10, 0.0);
        w.on_local(2_000, &local(15.0, -15.0, 0.0, 0.0), 2_000);
        let r = w.take().expect("a reset");
        assert!((geo::norm(r.shift) - 21.2).abs() < 0.1, "{r:?}");
        assert_eq!(r.cause, ResetCause::PositionStep);
        assert_eq!(w.take(), None, "taken once");
    }

    #[test]
    fn fast_flight_is_explained_by_its_velocity() {
        let mut w = ResetWatch::default();
        // 20 m/s at 10 Hz and at 2 Hz: 2 m and 10 m between samples.
        fly(&mut w, 1_000, 100, 50, 20.0);
        assert_eq!(w.take(), None);
        let mut w = ResetWatch::default();
        fly(&mut w, 1_000, 500, 20, 20.0);
        assert_eq!(w.take(), None);
        // Braking hard (8 m/s²) between 4 Hz samples.
        let mut w = ResetWatch::default();
        let (mut x, mut v) = (0.0f32, 16.0f32);
        for i in 0..8u32 {
            w.on_local(1_000 + i * 250, &local(x, 0.0, v, 0.0), 0);
            let v1 = (v - 8.0 * 0.25).max(0.0);
            x += 0.5 * (v + v1) * 0.25;
            v = v1;
        }
        assert_eq!(w.take(), None);
    }

    #[test]
    fn a_stepped_bench_clock_or_a_link_gap_is_not_judged() {
        let mut w = ResetWatch::default();
        fly(&mut w, 1_000, 100, 10, 5.0);
        // The bench clock steps 30 s on; the vehicle flew on meanwhile, the
        // velocities say nothing of the path.
        w.on_local(31_900, &local(40.0, 12.0, 0.0, 0.0), 1_900);
        // Then it runs on from there; the host clock stepped too.
        w.on_local(32_000, &local(40.0, 12.0, 0.0, 0.0), 90_000);
        // A reordered older sample, then the next one.
        w.on_local(31_950, &local(0.0, 0.0, 0.0, 0.0), 90_010);
        w.on_local(32_100, &local(40.0, 12.0, 0.0, 0.0), 90_015);
        // A rebooted FC.
        w.on_local(500, &local(-3.0, 0.0, 0.0, 0.0), 90_020);
        w.on_local(600, &local(-3.0, 0.0, 0.0, 0.0), 90_120);
        assert_eq!(w.take(), None);
        // No FC time at all (a FC or harness that never stamps).
        let mut w = ResetWatch::default();
        w.on_local(0, &local(0.0, 0.0, 0.0, 0.0), 0);
        w.on_local(0, &local(30.0, 0.0, 0.0, 0.0), 20);
        assert_eq!(w.take(), None);
    }

    #[test]
    fn a_signal_names_the_cause_and_lowers_the_step() {
        let mut w = ResetWatch::default();
        fly(&mut w, 1_000, 100, 10, 0.0);
        w.on_text("GPS Glitch or Compass error", true, 1_950);
        w.on_local(2_000, &local(0.8, 0.0, 0.0, 0.0), 2_000);
        let r = w.take().expect("a cued reset");
        assert_eq!(r.cause, ResetCause::GpsGlitch);
        // Past the window a 0.8 m step is no reset.
        w.on_local(8_100, &local(0.8, 0.0, 0.0, 0.0), 8_100);
        w.on_local(8_200, &local(1.6, 0.0, 0.0, 0.0), 8_200);
        assert_eq!(w.take(), None);
        // Disarmed, a text cues nothing.
        let mut w = ResetWatch::default();
        fly(&mut w, 1_000, 100, 10, 0.0);
        w.on_text("EKF3 IMU0 is using GPS", false, 1_950);
        w.on_local(2_000, &local(0.8, 0.0, 0.0, 0.0), 2_000);
        assert_eq!(w.take(), None);
    }

    #[test]
    fn the_origin_set_while_armed_or_moved_is_a_signal() {
        let mut w = ResetWatch::default();
        w.on_origin(473_000_000, 85_000_000, false, 0);
        assert_eq!(w.cue, None, "set on the ground");
        w.on_origin(473_000_000, 85_000_000, true, 10);
        assert_eq!(w.cue, None, "the same origin again");
        w.on_origin(473_000_100, 85_000_000, true, 20);
        assert_eq!(w.cue, Some((ResetCause::OriginSet, 20)));
        let mut w = ResetWatch::default();
        w.on_origin(1, 2, true, 30);
        assert_eq!(
            w.cue,
            Some((ResetCause::OriginSet, 30)),
            "first set in flight"
        );
    }

    #[test]
    fn texts_name_their_causes() {
        for (t, c) in [
            ("EKF3 IMU0 origin set", Some(ResetCause::OriginSet)),
            ("GPS Glitch or Compass error", Some(ResetCause::GpsGlitch)),
            ("Glitch cleared", Some(ResetCause::GpsGlitch)),
            ("EKF3 lane switch 1", Some(ResetCause::LaneSwitch)),
            ("EKF primary changed:1", Some(ResetCause::LaneSwitch)),
            ("EKF3 IMU0 is using GPS", Some(ResetCause::SourceSet)),
            (
                "EKF3 IMU1 is using external nav data",
                Some(ResetCause::SourceSet),
            ),
            ("EKF3 IMU0 fusing odometry", Some(ResetCause::SourceSet)),
            ("PreArm: Throttle below failsafe", None),
        ] {
            assert_eq!(ResetCause::of_text(t), c, "{t}");
        }
    }
}
