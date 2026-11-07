//! Multirotor stick model (C2): profiles, expo, a jerk-limited velocity
//! shaper, and position, altitude and heading hold capture.

use ados_protocol::pilot::Profile;

use crate::geo::{self, V2};

/// One profile's limits.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    pub v_h: f32,
    pub v_up: f32,
    pub v_down: f32,
    pub yaw_dps: f32,
    pub a_h: f32,
    pub jerk: f32,
    pub expo: f32,
}

pub fn limits(p: Profile) -> Limits {
    match p {
        Profile::Cine => Limits {
            v_h: 3.0,
            v_up: 1.5,
            v_down: 1.0,
            yaw_dps: 30.0,
            a_h: 1.5,
            jerk: 3.0,
            expo: 0.3,
        },
        Profile::Normal => Limits {
            v_h: 8.0,
            v_up: 3.0,
            v_down: 2.0,
            yaw_dps: 90.0,
            a_h: 3.0,
            jerk: 6.0,
            expo: 0.2,
        },
        Profile::Sport => Limits {
            v_h: 14.0,
            v_up: 5.0,
            v_down: 3.0,
            yaw_dps: 150.0,
            a_h: 6.0,
            jerk: 12.0,
            expo: 0.1,
        },
    }
}

/// `(1 - e)·x + e·x³`.
pub fn expo(x: f32, e: f32) -> f32 {
    let x = x.clamp(-1.0, 1.0);
    (1.0 - e) * x + e * x * x * x
}

/// Below this speed a centred stick captures a hold point.
pub const HOLD_CAPTURE_MPS: f32 = 0.2;

/// A 2-D velocity shaper: acceleration magnitude ≤ `a_h`, change of
/// acceleration ≤ `jerk`·dt.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Shaper {
    pub v: V2,
    pub a: V2,
}

impl Shaper {
    pub fn reset(&mut self, v: V2) {
        self.v = v;
        self.a = [0.0, 0.0];
    }

    /// Advance one step toward `target`.
    pub fn step(&mut self, target: V2, lim: &Limits, dt: f32) -> V2 {
        let err = geo::sub(target, self.v);
        let e = geo::norm(err);
        // Desired acceleration: the largest one that can still be ramped back to
        // zero by the jerk limit before reaching the target (sqrt(2·j·e)),
        // capped at a_h.
        let a_mag = (2.0 * lim.jerk * e).sqrt().min(lim.a_h);
        let a_des = if e > 1e-6 {
            geo::scale(err, a_mag / e)
        } else {
            [0.0, 0.0]
        };
        let da = geo::clamp_len(geo::sub(a_des, self.a), lim.jerk * dt);
        self.a = geo::clamp_len(geo::add(self.a, da), lim.a_h);
        let step = geo::scale(self.a, dt);
        // Never step past the target.
        if geo::norm(step) >= e {
            self.v = target;
            self.a = [0.0, 0.0];
        } else {
            self.v = geo::add(self.v, step);
        }
        self.v
    }
}

/// What the stick model asks the FC for this tick.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Command {
    /// Position hold: `[n, e, d]`, zero velocity, heading `yaw`.
    Hold { pos: [f32; 3], yaw: f32 },
    /// Velocity `[vn, ve, vd]` and yaw rate (rad/s).
    Velocity { vel: [f32; 3], yaw_rate: f32 },
}

/// Inputs for one stick tick.
#[derive(Debug, Clone, Copy)]
pub struct StickIn {
    /// `[roll, pitch, thr, yaw]` in [-1, 1].
    pub axes: [f32; 4],
    pub pos: [f32; 3],
    pub vel: [f32; 3],
    pub yaw: f32,
    pub dt: f32,
}

/// Multirotor stick state: shaper plus captured holds.
#[derive(Debug, Clone, Default)]
pub struct Multirotor {
    pub shaper: Shaper,
    pub hold_ne: Option<V2>,
    pub hold_d: Option<f32>,
    pub hold_yaw: Option<f32>,
}

impl Multirotor {
    /// Forget holds and match the shaper to the vehicle (on entering pilot).
    pub fn engage(&mut self, vel_ne: V2) {
        self.shaper.reset(vel_ne);
        self.hold_ne = None;
        self.hold_d = None;
        self.hold_yaw = None;
    }

    /// Capture a full hold at the current state.
    pub fn hold_here(&mut self, pos: [f32; 3], yaw: f32) {
        self.shaper.reset([0.0, 0.0]);
        self.hold_ne = Some([pos[0], pos[1]]);
        self.hold_d = Some(pos[2]);
        self.hold_yaw = Some(yaw);
    }

    /// The horizontal velocity the sticks ask for, in NED, before shaping.
    pub fn stick_velocity(axes: [f32; 4], yaw: f32, lim: &Limits) -> V2 {
        let fwd = expo(axes[1], lim.expo) * lim.v_h;
        let right = expo(axes[0], lim.expo) * lim.v_h;
        geo::body_to_ned(fwd, right, yaw)
    }

    /// One tick. `limit` post-processes the shaped horizontal velocity (the
    /// obstacle brake and the geofence), `vz_bounds` clamps the vertical rate
    /// (floor and ceiling).
    pub fn step(
        &mut self,
        inp: &StickIn,
        lim: &Limits,
        limit: impl Fn(V2) -> V2,
        vz_bounds: (f32, f32),
    ) -> Command {
        let [_, _, thr, yaw_in] = inp.axes;
        let horiz_centred = inp.axes[0] == 0.0 && inp.axes[1] == 0.0;
        // Horizontal.
        let target = if horiz_centred {
            [0.0, 0.0]
        } else {
            Self::stick_velocity(inp.axes, inp.yaw, lim)
        };
        if !horiz_centred {
            self.hold_ne = None;
        }
        let shaped = self.shaper.step(target, lim, inp.dt);
        let v_ne = limit(shaped);
        if geo::norm(v_ne) < geo::norm(shaped) {
            // The limiter cut speed: keep the shaper consistent with what flies.
            self.shaper.v = v_ne;
        }
        if horiz_centred && self.hold_ne.is_none() && geo::norm(self.shaper.v) < HOLD_CAPTURE_MPS {
            self.hold_ne = Some([inp.pos[0], inp.pos[1]]);
            self.shaper.reset([0.0, 0.0]);
        }
        // Vertical (NED: down positive).
        let vd = if thr == 0.0 {
            let hold = *self.hold_d.get_or_insert(inp.pos[2]);
            (hold - inp.pos[2]).clamp(-lim.v_up, lim.v_down)
        } else {
            self.hold_d = None;
            let t = expo(thr, lim.expo);
            if t > 0.0 {
                -t * lim.v_up
            } else {
                -t * lim.v_down
            }
        };
        let vd = vd.clamp(vz_bounds.0, vz_bounds.1);
        // Yaw.
        let yaw_rate = if yaw_in == 0.0 {
            let hold = *self.hold_yaw.get_or_insert(inp.yaw);
            (geo::wrap_pi(hold - inp.yaw) * 2.0)
                .clamp(-lim.yaw_dps.to_radians(), lim.yaw_dps.to_radians())
        } else {
            self.hold_yaw = None;
            expo(yaw_in, lim.expo) * lim.yaw_dps.to_radians()
        };
        match (self.hold_ne, self.hold_d, self.hold_yaw) {
            (Some(ne), Some(d), Some(yaw)) if vz_bounds.0 <= 0.0 && vz_bounds.1 >= 0.0 => {
                Command::Hold {
                    pos: [ne[0], ne[1], d],
                    yaw,
                }
            }
            _ => Command::Velocity {
                vel: [v_ne[0], v_ne[1], vd],
                yaw_rate,
            },
        }
    }
}

#[cfg(test)]
mod tests;
