//! The operator's sticks on the pilot frame: the limits they fly at now and
//! the command of the latest stick tick.

use serde::{Deserialize, Serialize};

use super::state::Profile;

/// The limits the pilot flies the operator's sticks at now: the profile's
/// row, never above the flight controller's own guidance limits, the
/// vehicle's limits, the place's caps or a speed cap.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct ProfileLimits {
    pub profile: Profile,
    /// Horizontal speed at full stick, m/s.
    pub v_h_mps: f32,
    /// Climb and descent speed at full stick, m/s.
    pub v_up_mps: f32,
    pub v_down_mps: f32,
    /// Yaw rate at full stick, °/s.
    pub yaw_dps: f32,
    /// Horizontal acceleration while speeding up, and deceleration while
    /// slowing or braking to a hold, m/s².
    pub a_acc_mps2: f32,
    pub a_brk_mps2: f32,
    /// Horizontal jerk, m/s³.
    pub jerk_mps3: f32,
    /// Vertical acceleration, m/s².
    pub a_v_mps2: f32,
    /// Yaw acceleration, °/s².
    pub yaw_dps2: f32,
    /// Stick expo per axis: `(1-e)·x + e·x³`.
    pub expo_h: f32,
    pub expo_v: f32,
    pub expo_yaw: f32,
    /// Distance to stop from `v_h_mps` on release, m:
    /// `v²/(2·a_brk) + v·a_brk/(2·jerk) + v·0.15 s`.
    pub stop_m: f32,
    /// The obstacle margin the pilot keeps, m (to colour
    /// `min_clearance_m`).
    #[serde(default)]
    pub margin_m: f32,
}

/// What the operator's sticks ask and what the pilot flies, on its latest
/// stick tick.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct StickCommand {
    /// The asked velocity after the centre threshold and expo, before
    /// shaping: north-east m/s, vertical m/s (+ up) and yaw rate °/s
    /// (+ clockwise).
    pub ask_ne_mps: [f32; 2],
    pub ask_vz_mps: f32,
    pub ask_yaw_dps: f32,
    /// The shaped reference sent: north-east velocity and acceleration,
    /// vertical speed (+ up) and yaw rate.
    pub v_ne_mps: [f32; 2],
    pub a_ne_mps2: [f32; 2],
    pub vz_mps: f32,
    pub yaw_rate_dps: f32,
    /// The reference brakes to a hold (sticks released, or the controller
    /// lost).
    pub braking: bool,
    /// Age of the stick frame flown this tick, ms, and its sequence number.
    pub input_age_ms: u32,
    pub input_seq: u32,
}

/// The latency a stop distance allows for, s.
pub const STOP_LATENCY_S: f32 = 0.15;

impl ProfileLimits {
    /// Distance to stop from `v` m/s braking at `a_brk` m/s² with jerk
    /// `jerk` m/s³, after [`STOP_LATENCY_S`]:
    /// `v²/(2·a_brk) + v·a_brk/(2·jerk) + v·t_lat`.
    pub fn stop_distance_m(v: f32, a_brk: f32, jerk: f32) -> f32 {
        if v <= 0.0 || a_brk <= 0.0 || jerk <= 0.0 {
            return 0.0;
        }
        v * v / (2.0 * a_brk) + v * a_brk / (2.0 * jerk) + v * STOP_LATENCY_S
    }
}
