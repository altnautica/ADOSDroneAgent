//! The attitude/body-rate command set and its saturation ceilings.
//!
//! [`AttitudeCommand`] is the pure, transport-independent body-rate + thrust
//! command a control law emits. The router rung converts it to a
//! `SET_ATTITUDE_TARGET` (id 82) through `ados_protocol`, so this crate never
//! sees a MAVLink type.
//!
//! The ceilings here mirror `ados-swarm-control`'s [`MAX_COMMAND_SPEED_MPS`]: a
//! proportional control law over an error that can be large needs a hard ceiling
//! so it saturates into a rate the airframe can actually hold rather than asking
//! for something the aircraft would tear itself apart trying. They are plain
//! values, not a policy: the router decides whether to command at all.

/// Ceiling on any commanded body rate, rad/s (about ±343 °/s).
///
/// Every proportional law over an attitude error that can be large saturates
/// here. Chosen as a rate a small multirotor can meet and hold without hitting
/// its own physical limits.
pub const MAX_BODY_RATE_RAD_S: f32 = 6.0;

/// The minimum commanded collective thrust (normalized).
pub const MIN_THRUST: f32 = 0.0;
/// The maximum commanded collective thrust (normalized), the top of the
/// `SET_ATTITUDE_TARGET` normalized range.
pub const MAX_THRUST: f32 = 1.0;

/// A single body-rate + thrust command, the output of this crate's control laws.
///
/// Frame-independent and transport-independent: it is exactly the meaningful
/// payload of a body-rate `SET_ATTITUDE_TARGET` (quaternion ignored). The router
/// packs it into the wire message and owns the sending policy.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AttitudeCommand {
    /// Body roll rate command, rad/s (right-hand rule).
    pub body_roll_rate: f32,
    /// Body pitch rate command, rad/s.
    pub body_pitch_rate: f32,
    /// Body yaw rate command, rad/s.
    pub body_yaw_rate: f32,
    /// Collective thrust command, normalized `0..=1`.
    pub thrust: f32,
}

impl AttitudeCommand {
    /// The zero-rate, minimum-thrust placeholder the router holds when no rate
    /// command is live. It is never emitted: the router's gate suppresses every
    /// tick without a live, attested command, and zero thrust on an airborne
    /// multirotor is a motor cut, not a hold.
    pub const fn neutral() -> Self {
        Self {
            body_roll_rate: 0.0,
            body_pitch_rate: 0.0,
            body_yaw_rate: 0.0,
            thrust: MIN_THRUST,
        }
    }
}

/// Whether a normalized thrust is within the commandable range.
pub fn thrust_range_ok(thrust: f32) -> bool {
    thrust.is_finite() && (MIN_THRUST..=MAX_THRUST).contains(&thrust)
}

/// Clamp a raw body-rate command into the [`MAX_BODY_RATE_RAD_S`] ceiling,
/// saturating rather than wrapping or rejecting. A rate command over the
/// ceiling is a request the airframe cannot meet, and stringing it through
/// unresisted would let a large attitude error command a violence the aircraft
/// cannot hold.
pub fn body_rate_ceiling(rate: f32) -> f32 {
    rate.clamp(-MAX_BODY_RATE_RAD_S, MAX_BODY_RATE_RAD_S)
}

/// Build a [`AttitudeCommand`] from raw body rates, ceiling-clamped, with the
/// thrust kept inside the normalized commandable range. A finite rate outside
/// the ceiling saturates; a finite thrust outside the range saturates to the
/// nearest bound.
///
/// `None` when any input is NaN or infinite. A control law that produced one has
/// nothing trustworthy to command, and this layer then emits nothing so the
/// router suppresses and the FC holds. Substituting a value would be worse than
/// silence: a zero stand-in for thrust commands the motors to idle in flight.
pub fn rate_command(roll: f32, pitch: f32, yaw: f32, thrust: f32) -> Option<AttitudeCommand> {
    if ![roll, pitch, yaw, thrust].iter().all(|v| v.is_finite()) {
        return None;
    }
    Some(AttitudeCommand {
        body_roll_rate: body_rate_ceiling(roll),
        body_pitch_rate: body_rate_ceiling(pitch),
        body_yaw_rate: body_rate_ceiling(yaw),
        thrust: thrust.clamp(MIN_THRUST, MAX_THRUST),
    })
}

/// Cumulative counters the router rung publishes into its atomics status block,
/// so a status consumer can see where the lane's time went.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AttitudeControlCounters {
    /// `SET_ATTITUDE_TARGET` frames actually emitted toward the FC.
    pub setpoints_emitted: u64,
    /// Ticks that were suppressed entirely (not emitted) — includes the
    /// freshness-gate suppressions and any no-command ticks.
    pub ticks_suppressed: u64,
    /// Ticks suppressed specifically because the own-attitude fix was stale —
    /// the freshness gate that means a stale attitude never commands.
    pub freshness_suppressions: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neutral_is_zero_rate_minimum_thrust() {
        let n = AttitudeCommand::neutral();
        assert_eq!(n.body_roll_rate, 0.0);
        assert_eq!(n.body_pitch_rate, 0.0);
        assert_eq!(n.body_yaw_rate, 0.0);
        assert_eq!(n.thrust, MIN_THRUST);
    }

    #[test]
    fn rates_saturate_into_the_ceiling() {
        assert_eq!(body_rate_ceiling(100.0), MAX_BODY_RATE_RAD_S);
        assert_eq!(body_rate_ceiling(-100.0), -MAX_BODY_RATE_RAD_S);
        assert_eq!(body_rate_ceiling(1.0), 1.0);
    }

    #[test]
    fn rate_command_saturates_each_axis_and_thrust() {
        let c = rate_command(999.0, -999.0, 2.0, 5.0).unwrap();
        assert_eq!(c.body_roll_rate, MAX_BODY_RATE_RAD_S);
        assert_eq!(c.body_pitch_rate, -MAX_BODY_RATE_RAD_S);
        assert_eq!(c.body_yaw_rate, 2.0);
        assert_eq!(c.thrust, MAX_THRUST);

        let low = rate_command(0.0, 0.0, 0.0, -3.0).unwrap();
        assert_eq!(low.thrust, MIN_THRUST);
    }

    /// A non-finite input is suppressed, never turned into a command: a NaN
    /// thrust replaced by zero would idle the motors in flight.
    #[test]
    fn a_non_finite_input_yields_no_command() {
        assert_eq!(rate_command(0.1, 0.0, 0.0, f32::NAN), None);
        assert_eq!(rate_command(0.1, 0.0, 0.0, f32::INFINITY), None);
        assert_eq!(rate_command(f32::NAN, 0.0, 0.0, 0.5), None);
        assert_eq!(rate_command(0.0, f32::NEG_INFINITY, 0.0, 0.5), None);
    }

    #[test]
    fn thrust_range_predicate() {
        assert!(thrust_range_ok(0.0));
        assert!(thrust_range_ok(1.0));
        assert!(!thrust_range_ok(1.5));
        assert!(!thrust_range_ok(-0.1));
        assert!(!thrust_range_ok(f32::NAN));
    }
}
