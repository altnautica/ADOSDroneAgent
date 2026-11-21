//! The attitude a scan was taken at. A planar scan is levelled with the
//! vehicle's roll and pitch ([`crate::obstacles::Level`]); while the vehicle
//! leans and rights itself the attitude at the scan's capture can differ
//! from the newest one by degrees, a scan's transport alone taking tens of
//! milliseconds. Each FC ATTITUDE is logged with the host CLOCK_MONOTONIC
//! of its arrival, the clock the lidar driver stamps a scan's capture on
//! (`t_ns`), and a scan is levelled with the attitude logged at its
//! capture. The host clock is read through [`super::Pilot::mono_ns`], so
//! the core stays pure: unset (tests, the in-process simulation) nothing is
//! logged and every scan takes the newest attitude.

use std::collections::VecDeque;

use crate::fc::Attitude;
use crate::obstacles::{Climb, Level};

/// A commanded climb this old no longer counts toward the band, ms.
const CLIMB_FRESH_MS: u64 = 200;

impl super::Pilot {
    /// Log the FC's newest attitude at the host clock now.
    pub(super) fn log_attitude(&mut self) {
        if let Some(a) = self.fc.attitude {
            self.att_log.push((self.mono_ns)(), a.v);
        }
    }

    /// How to level the scan `payload`: the attitude at its capture stamp
    /// (`t_ns`, host CLOCK_MONOTONIC) when the log holds it, else the
    /// newest; the height over the ground from the downward rangefinder
    /// (its slant levelled with that attitude) while it is fresh and in its
    /// trusted range, else the height the pilot flies on.
    pub(super) fn scan_level(&self, payload: &serde_json::Value, now_ms: u64) -> Level {
        let att = payload
            .get("t_ns")
            .and_then(serde_json::Value::as_u64)
            .and_then(|t| self.att_log.at(t))
            .or_else(|| self.fc.fresh_attitude(now_ms))
            .unwrap_or_default();
        let agl = self
            .fc
            .rangefinder_agl(now_ms)
            .map(|r| r * att.roll.cos() * att.pitch.cos())
            .or_else(|| self.agl(now_ms));
        Level {
            roll: att.roll,
            pitch: att.pitch,
            yaw: att.yaw,
            agl,
        }
    }

    /// The climb the swept band rises with: the faster of the climb flown
    /// and the one the setpoint sent asks, up to the height that setpoint
    /// still lies above (none without a fresh one: the FC's own climb ends
    /// where it ends).
    pub(super) fn band_climb(&self, now_ms: u64) -> Climb {
        let local = self.fc.fresh_local(now_ms);
        let flown_up = local.map_or(0.0, |l| -l.vd);
        let sent = self
            .command
            .filter(|c| now_ms.saturating_sub(c.at_ms) <= CLIMB_FRESH_MS);
        let asked_up = sent.map_or(0.0, |c| -c.sp_vel[2]);
        let up_to = match (sent, self.setpoint, local) {
            (Some(_), Some(sp), Some(l)) => l.d - sp[2],
            _ => 0.0,
        };
        let h = local.map_or(0.0, |l| l.vn.hypot(l.ve));
        Climb::of(flown_up.max(asked_up), h, up_to)
    }
}

/// ATTITUDE samples kept: over a second at the FC's 50 Hz.
const LOG_LEN: usize = 64;
/// A capture this far past the first or last sample still takes the
/// nearest one, ns.
const MATCH_NS: u64 = 60_000_000;

/// FC attitudes by host-monotonic arrival, oldest first.
#[derive(Debug, Default)]
pub struct AttitudeLog {
    ring: VecDeque<(u64, Attitude)>,
}

impl AttitudeLog {
    /// Log `a`, arrived at host-monotonic `mono_ns` (0: no host clock).
    pub fn push(&mut self, mono_ns: u64, a: Attitude) {
        if mono_ns == 0 || self.ring.back().is_some_and(|(t, _)| *t > mono_ns) {
            return;
        }
        if self.ring.len() == LOG_LEN {
            self.ring.pop_front();
        }
        self.ring.push_back((mono_ns, a));
    }

    /// The attitude at host-monotonic `t_ns`: interpolated between the two
    /// samples around it, the nearest one within [`MATCH_NS`] past either
    /// end, `None` further out.
    pub fn at(&self, t_ns: u64) -> Option<Attitude> {
        let (first, last) = (self.ring.front()?, self.ring.back()?);
        if t_ns + MATCH_NS < first.0 || t_ns > last.0 + MATCH_NS {
            return None;
        }
        if t_ns <= first.0 {
            return Some(first.1);
        }
        if t_ns >= last.0 {
            return Some(last.1);
        }
        let i = self.ring.partition_point(|(t, _)| *t <= t_ns);
        let ((t0, a), (t1, b)) = (self.ring[i - 1], self.ring[i]);
        let k = (t_ns - t0) as f32 / (t1 - t0).max(1) as f32;
        let lerp = |x: f32, y: f32| x + (y - x) * k;
        let turn = crate::geo::wrap_pi(b.yaw - a.yaw);
        Some(Attitude {
            roll: lerp(a.roll, b.roll),
            pitch: lerp(a.pitch, b.pitch),
            yaw: crate::geo::wrap_pi(a.yaw + turn * k),
            yaw_rate: lerp(a.yaw_rate, b.yaw_rate),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn att(pitch_deg: f32) -> Attitude {
        Attitude {
            pitch: pitch_deg.to_radians(),
            ..Attitude::default()
        }
    }

    /// A lean righting at 60°/s: a scan captured 100 ms before it arrived
    /// takes the pitch of its capture, not the newest.
    #[test]
    fn a_scan_takes_the_attitude_of_its_capture() {
        let mut log = AttitudeLog::default();
        for k in 0..20u64 {
            log.push(1_000_000_000 + k * 20_000_000, att(20.0 - 1.2 * k as f32));
        }
        let p = log
            .at(1_000_000_000 + 280_000_000)
            .unwrap()
            .pitch
            .to_degrees();
        assert!((p - 3.2).abs() < 1e-3, "{p}");
        let mid = log
            .at(1_000_000_000 + 290_000_000)
            .unwrap()
            .pitch
            .to_degrees();
        assert!((mid - 2.6).abs() < 1e-3, "interpolated {mid}");
        assert!(log.at(1_000_000_000 + 380_000_000 + MATCH_NS + 1).is_none());
        assert!(log.at(1_000_000_000 - MATCH_NS - 1).is_none());
        let mut none = AttitudeLog::default();
        none.push(0, att(5.0));
        assert!(none.at(0).is_none(), "no host clock, no log");
    }
}
