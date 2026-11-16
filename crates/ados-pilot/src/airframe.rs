//! Airframe health: the trim an unbalanced airframe needs, learned from its
//! own flights and kept per flight controller, and whether it has the motor
//! headroom to fly at all.
//!
//! The supervised lift-off loads the learned trim into the FC's rate loop on
//! the ground ([`Airframe::trim`]); without one it finds it on the ground
//! (probe) and reports what it found ([`Airframe::note_liftoff`]).

use ados_protocol::pilot::Readiness;

use crate::fc::FcState;

/// The rate-loop integrators an airframe holds in steady hover, and its
/// hover throttle.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Trim {
    /// Roll and pitch rate-loop I (PID_TUNING), in the FC's output units.
    pub i: [f32; 2],
    /// Hover throttle, 0..1.
    pub hover_thr: f32,
    /// Steady-hover samples behind the values (0: a lift-off probe only).
    pub samples: u32,
}

/// What a supervised lift-off found, for airframe health.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LiftoffOutcome {
    /// The roll and pitch I at lift-off (`None`: it never lifted).
    pub i: Option<[f32; 2]>,
    /// The thrust it lifted at, 0..1.
    pub thrust: f32,
    /// The busiest motor's share of its output range before lift-off, 0..1.
    pub max_motor: f32,
    /// It stopped because a motor reached the top of its range.
    pub saturated: bool,
}

/// Airframe health for the connected FC.
#[derive(Debug, Clone, Default)]
pub struct Airframe {
    trim: Option<Trim>,
}

impl Airframe {
    /// The learned trim for the connected FC; `None` until one is known (the
    /// lift-off then probes).
    pub fn trim(&self) -> Option<Trim> {
        self.trim
    }

    /// Fold one tick of FC state in (steady-hover learning).
    pub fn tick(&mut self, _fc: &FcState, _now_ms: u64) {}

    /// Record a supervised lift-off's outcome.
    pub fn note_liftoff(&mut self, o: &LiftoffOutcome, _now_ms: u64) {
        if let Some(i) = o.i {
            self.trim = Some(Trim {
                i,
                hover_thr: o.thrust,
                samples: 0,
            });
        }
    }

    /// The imbalance in operator words ("nose-heavy by about 35 mm, move the
    /// battery back"), `None` while balanced or unknown.
    pub fn imbalance_text(&self, _fc: &FcState) -> Option<String> {
        None
    }

    /// The `airframe_balance` readiness row, `None` while nothing is known.
    pub fn readiness(&self, _fc: &FcState) -> Option<Readiness> {
        None
    }
}

/// The imbalance the roll and pitch integrators `i` hold at thrust `thrust`,
/// in operator words ("nose-heavy by about 35 mm, move the battery back"),
/// `None` while within the balance tolerance or the geometry is unknown.
pub fn imbalance_words(_fc: &FcState, _i: [f32; 2], _thrust: f32) -> Option<String> {
    None
}
