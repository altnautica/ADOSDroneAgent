//! A test airframe on its skids under ArduPilot's GUIDED attitude control,
//! for the lift-off's tests and the pilot's test world: rate-loop
//! integrators that wind up against an attitude error, a centre of gravity
//! off the thrust centre, skids that resist a tip until the thrust unloads
//! them, motor outputs that saturate, and a climb once thrust passes hover.
//!
//! It is a behavioural model, not a physical one: the numbers are chosen so
//! the World Engine airframes' measured signs and orders of magnitude hold
//! (a 0.25 trim tips the airframe on its skids from ~85 % of hover, the
//! integrator winds at ~0.06 /s for a 6° lean, a 0.37 trim leaves the
//! busiest motor above 90 % at lift-off thrust).

/// Integrator wind per rad of attitude error, per second.
const I_PER_RAD_S: f32 = 0.61;
/// The skids resist a trim error this large at zero thrust, falling to
/// none at hover.
const SKID_RESIST: f32 = 1.5;
/// Tilt rate per unit of unresisted trim error, rad/s.
const TIP_RATE: f32 = 3.0;
/// The attitude loop's pull toward its target in the air, 1/s.
const ATT_GAIN: f32 = 4.0;

/// One airframe.
#[derive(Debug, Clone, PartialEq)]
pub struct Airframe {
    /// Hover thrust, 0..1.
    pub hover: f32,
    /// The roll and pitch integrators it needs in the air.
    pub trim: [f32; 2],
    /// Roll, pitch (rad) and yaw.
    pub att: [f32; 3],
    /// Roll and pitch rate-loop integrators.
    pub i: [f32; 2],
    /// Height above the ground (m, up) and climb rate (m/s, up).
    pub h: f32,
    pub vz: f32,
    /// The commanded attitude and thrust; `None` until the first target.
    pub target: Option<([f32; 3], f32)>,
    /// A position target's height (m, up), followed once airborne.
    pub goal_h: Option<f32>,
    /// Armed and spinning (the integrators run).
    pub armed: bool,
    /// Flipped past 60°: crashed.
    pub crashed: bool,
}

impl Airframe {
    pub fn new(hover: f32, trim: [f32; 2]) -> Self {
        Self {
            hover,
            trim,
            att: [0.0; 3],
            i: [0.0; 2],
            h: 0.0,
            vz: 0.0,
            target: None,
            goal_h: None,
            armed: false,
            crashed: false,
        }
    }

    /// A balanced airframe hovering at half thrust.
    pub fn balanced() -> Self {
        Self::new(0.5, [0.0; 2])
    }

    pub fn airborne(&self) -> bool {
        self.h > 0.0
    }

    /// The thrust it flies with, 0..1.
    pub fn thrust(&self) -> f32 {
        self.target.map_or(0.0, |t| t.1)
    }

    /// Motor outputs (µs) of a quad X: front pair, back pair.
    pub fn motors(&self) -> [u16; 4] {
        if !self.armed {
            return [1000; 4];
        }
        let base = 0.1 + 0.9 * self.thrust();
        let us = |f: f32| (1000.0 + 1000.0 * f.clamp(0.0, 1.0)) as u16;
        // A nose-up integrator (positive pitch I) drives the front motors.
        let p = 0.6 * self.i[1];
        let r = 0.6 * self.i[0];
        [
            us(base + p - r),
            us(base - p + r),
            us(base + p + r),
            us(base - p - r),
        ]
    }

    /// Advance `dt` seconds.
    pub fn step(&mut self, dt: f32) {
        if !self.armed || self.crashed {
            self.i = [0.0; 2];
            return;
        }
        let Some((tgt, thrust)) = self.target else {
            return;
        };
        let frac = thrust / self.hover;
        for ax in 0..2 {
            self.i[ax] += dt * I_PER_RAD_S * (tgt[ax] - self.att[ax]);
            // Too little integrator drops that side (negative tilt).
            let err = self.i[ax] - self.trim[ax];
            if self.airborne() {
                self.att[ax] += dt * (ATT_GAIN * (tgt[ax] - self.att[ax]) + TIP_RATE * err);
            } else {
                let resist = (SKID_RESIST * (1.0 - frac)).max(0.0);
                let free = err.abs() - resist;
                if free > 0.0 {
                    self.att[ax] += dt * TIP_RATE * free * err.signum();
                } else {
                    // Back onto the skids as the thrust unloads.
                    self.att[ax] -= self.att[ax] * (5.0 * dt).min(1.0);
                }
            }
            if self.att[ax].abs() > 1.05 {
                self.crashed = true;
            }
        }
        let lift = thrust * self.att[0].cos() * self.att[1].cos() / self.hover - 1.0;
        match self.goal_h.filter(|_| self.airborne()) {
            Some(g) => {
                self.vz = (g - self.h).clamp(-1.0, 1.0);
            }
            None => self.vz += dt * 9.81 * lift,
        }
        self.h += dt * self.vz;
        if self.h <= 0.0 {
            self.h = 0.0;
            self.vz = self.vz.max(0.0);
            if lift <= 0.0 {
                self.vz = 0.0;
            }
        }
    }
}
