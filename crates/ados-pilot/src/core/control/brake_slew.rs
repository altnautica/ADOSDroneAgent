//! The obstacle brake's own answer, slewed. A push out of the margin, or
//! the start back the other way once a brake has stopped the vehicle,
//! stepped the velocity setpoint at once, and the FC flew the step at its
//! lean limit: under a canopy the stop from 2 m/s and the push back out
//! swung the copter past 20° each way. Now the answer may speed up, turn
//! or push away from the velocity last sent only at the goal limits'
//! acceleration. A cut in the speed flown still holds at once: it is what
//! keeps the stop inside the room the brake's caps leave.

use crate::geo::{self, V2};

/// The velocity last sent counts for this long, ms; older, the FC's own.
const SENT_FRESH_MS: u64 = 100;

/// `answer` reached from `prev` over `dt` seconds: the speed along `prev`
/// falls to the answer's at once and rises at most `a_h`; the rest of the
/// change (turning, pushing, starting from still) at most `a_h`.
pub(crate) fn slew(prev: V2, answer: V2, a_h: f32, dt: f32) -> V2 {
    let speed = geo::norm(prev);
    if speed < 1e-3 {
        return geo::add(prev, geo::clamp_len(geo::sub(answer, prev), a_h * dt));
    }
    let e = geo::scale(prev, 1.0 / speed);
    let along = geo::dot(answer, e);
    // Slowing holds at once (down to a stop, never past it into reverse);
    // speeding up and reversing ramp.
    let flown = if along < speed {
        along.max(0.0).min(speed)
    } else {
        along.min(speed + a_h * dt)
    };
    let flown = if along < 0.0 {
        flown + along.max(-a_h * dt)
    } else {
        flown
    };
    let side = geo::sub(answer, geo::scale(e, along));
    geo::add(geo::scale(e, flown), geo::clamp_len(side, a_h * dt))
}

impl crate::core::Pilot {
    /// The brake's `answer` slewed ([`slew`]) from the horizontal velocity
    /// last sent (the newer of the last slewed answer and the last setpoint;
    /// else the FC's), one tick of `dt` seconds on.
    pub(super) fn brake_slew(&mut self, answer: V2, a_h: f32, dt: f32, now_ms: u64) -> V2 {
        let fresh = |at: u64| now_ms.saturating_sub(at) <= SENT_FRESH_MS;
        let sent = self
            .prev_command
            .filter(|c| fresh(c.at_ms))
            .map(|c| ([c.sp_vel[0], c.sp_vel[1]], c.at_ms));
        let slewed = self.brake_sent.filter(|(_, at)| fresh(*at));
        let prev = match (slewed, sent) {
            (Some((v, a)), Some((_, b))) if a >= b => Some(v),
            (_, Some((v, _))) => Some(v),
            (Some((v, _)), None) => Some(v),
            (None, None) => None,
        }
        .or_else(|| self.fc.fresh_local(now_ms).map(|l| [l.vn, l.ve]))
        .unwrap_or([0.0; 2]);
        let v = slew(prev, answer, a_h, dt);
        self.brake_sent = Some((v, now_ms));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cut_holds_at_once_and_a_push_or_reversal_ramps() {
        let dt = 0.02;
        // A cut to zero from 2.3 m/s north: at once.
        assert_eq!(slew([2.3, 0.0], [0.0, 0.0], 2.5, dt), [0.0, 0.0]);
        // A reversal: stops at once, then backs off at the goal rate.
        let v = slew([1.0, 0.0], [-0.9, 0.0], 2.5, dt);
        assert!((v[0] + 0.05).abs() < 1e-5, "{v:?}");
        // A push sideways out of the margin from still.
        let v = slew([0.0, 0.0], [0.0, 0.5], 2.5, dt);
        assert!((v[1] - 0.05).abs() < 1e-5, "{v:?}");
        // Speeding up from 1 m/s toward 2: at the goal rate.
        let v = slew([1.0, 0.0], [2.0, 0.0], 2.5, dt);
        assert!((v[0] - 1.05).abs() < 1e-5, "{v:?}");
        // Within reach in a tick: the answer itself.
        let v = slew([1.0, 0.0], [0.97, 0.01], 2.5, dt);
        assert!(geo::dist(v, [0.97, 0.01]) < 1e-5, "{v:?}");
    }
}
