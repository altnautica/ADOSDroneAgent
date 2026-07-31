//! The one command this layer emits: `SET_POSITION_TARGET_GLOBAL_INT`.
//!
//! MAVLink message 86, sent only while the FC reports GUIDED.
//! This module owns the field set and the `type_mask` semantics as pure data so
//! the encoding is unit-testable without a flight controller, a serial port or a
//! MAVLink dialect in scope; the router turns a [`Setpoint`] into the wire
//! message.

use crate::geo::{deg_to_e7, GeoOrigin, Ned};

/// Ceiling on any commanded speed, m/s.
///
/// Every control law is a proportional term over an error that can be hundreds
/// of metres, so each saturates here. Chosen as a conservative small-multirotor
/// cruise: high enough that a 500 m repositioning is a minute rather than ten,
/// low enough that the closing-rate barrier — whose allowance is 4 m/s at the
/// soft separation radius — remains the binding constraint near other aircraft.
pub const MAX_COMMAND_SPEED_MPS: f64 = 10.0;

/// `MAV_FRAME_GLOBAL_RELATIVE_ALT_INT`. Relative altitude, matching the
/// `alt_rel` this crate reasons about and the beacon's `alt_dm`.
pub const MAV_FRAME_GLOBAL_RELATIVE_ALT_INT: u8 = 6;

// POSITION_TARGET_TYPEMASK bits.
const X_IGNORE: u16 = 1;
const Y_IGNORE: u16 = 2;
const Z_IGNORE: u16 = 4;
const VX_IGNORE: u16 = 8;
const VY_IGNORE: u16 = 16;
const VZ_IGNORE: u16 = 32;
const AX_IGNORE: u16 = 64;
const AY_IGNORE: u16 = 128;
const AZ_IGNORE: u16 = 256;
const YAW_IGNORE: u16 = 1024;
const YAW_RATE_IGNORE: u16 = 2048;

/// Acceleration and attitude are never commanded by this layer.
const ALWAYS_IGNORED: u16 = AX_IGNORE | AY_IGNORE | AZ_IGNORE | YAW_IGNORE | YAW_RATE_IGNORE;

/// Which half of the message is meaningful.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetpointKind {
    /// Velocity control: `vn`/`ve`/`vd` are the command, position is ignored.
    /// The natural output of a potential-field law, and what every layer here
    /// emits while it is manoeuvring.
    Velocity,
    /// Position control: `lat`/`lon`/`alt` are the command, velocity is ignored.
    /// Used for a station hold, where a position target is what stops the vehicle
    /// drifting on residual velocity.
    Position,
}

/// One `SET_POSITION_TARGET_GLOBAL_INT` payload, frame-independent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Setpoint {
    pub kind: SetpointKind,
    /// Degrees × 1e7, MAVLink fixed point.
    pub lat_e7: i32,
    pub lon_e7: i32,
    /// Metres, relative to home (see [`MAV_FRAME_GLOBAL_RELATIVE_ALT_INT`]).
    pub alt_m: f32,
    /// Metres/second NED. `vd` is DOWN-positive: a climb is negative.
    pub vn: f32,
    pub ve: f32,
    pub vd: f32,
}

impl Setpoint {
    /// A velocity command. Position fields are zeroed because the mask ignores
    /// them; leaving a stale position in an ignored field is how a mask
    /// regression turns into an unexplained flyaway.
    pub fn velocity(cmd: Ned) -> Self {
        let c = cmd.clamp_norm(MAX_COMMAND_SPEED_MPS);
        Self {
            kind: SetpointKind::Velocity,
            lat_e7: 0,
            lon_e7: 0,
            alt_m: 0.0,
            vn: c.n as f32,
            ve: c.e as f32,
            vd: c.d as f32,
        }
    }

    /// A position hold at a geodetic fix.
    pub fn position(lat_deg: f64, lon_deg: f64, alt_m: f64) -> Self {
        Self {
            kind: SetpointKind::Position,
            lat_e7: deg_to_e7(lat_deg),
            lon_e7: deg_to_e7(lon_deg),
            alt_m: alt_m as f32,
            vn: 0.0,
            ve: 0.0,
            vd: 0.0,
        }
    }

    /// A position hold at a local-frame offset from `origin`.
    pub fn position_at(origin: &GeoOrigin, offset: Ned) -> Self {
        let (lat, lon, alt) = origin.to_geo(offset);
        Self::position(lat, lon, alt)
    }

    /// The `type_mask` for this setpoint's kind.
    pub const fn type_mask(&self) -> u16 {
        match self.kind {
            SetpointKind::Velocity => X_IGNORE | Y_IGNORE | Z_IGNORE | ALWAYS_IGNORED,
            SetpointKind::Position => VX_IGNORE | VY_IGNORE | VZ_IGNORE | ALWAYS_IGNORED,
        }
    }

    /// The coordinate frame every setpoint from this layer uses.
    pub const fn coordinate_frame(&self) -> u8 {
        MAV_FRAME_GLOBAL_RELATIVE_ALT_INT
    }

    /// Whether every field the mask declares meaningful is usable. A setpoint
    /// that fails this must never reach the FC.
    ///
    /// For a position hold, an all-zero fix is treated as invalid: null island is
    /// not a place this layer ever intends to fly to, so an all-zero lat/lon is
    /// uninitialised data rather than a command.
    pub fn is_valid(&self) -> bool {
        match self.kind {
            SetpointKind::Velocity => {
                self.vn.is_finite() && self.ve.is_finite() && self.vd.is_finite()
            }
            SetpointKind::Position => {
                self.alt_m.is_finite() && (self.lat_e7 != 0 || self.lon_e7 != 0)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn velocity_mask_ignores_position_and_commands_velocity() {
        let s = Setpoint::velocity(Ned::new(1.0, 2.0, -3.0));
        let m = s.type_mask();
        assert_eq!(m & X_IGNORE, X_IGNORE);
        assert_eq!(m & Y_IGNORE, Y_IGNORE);
        assert_eq!(m & Z_IGNORE, Z_IGNORE);
        assert_eq!(
            m & (VX_IGNORE | VY_IGNORE | VZ_IGNORE),
            0,
            "velocity must be honoured"
        );
        assert_eq!(m, 3527, "the pinned wire value");
    }

    #[test]
    fn position_mask_ignores_velocity_and_commands_position() {
        let s = Setpoint::position(12.9716, 77.5946, 30.0);
        let m = s.type_mask();
        assert_eq!(
            m & (VX_IGNORE | VY_IGNORE | VZ_IGNORE),
            VX_IGNORE | VY_IGNORE | VZ_IGNORE
        );
        assert_eq!(
            m & (X_IGNORE | Y_IGNORE | Z_IGNORE),
            0,
            "position must be honoured"
        );
        assert_eq!(m, 3576, "the pinned wire value");
    }

    #[test]
    fn acceleration_and_yaw_are_never_commanded() {
        for s in [
            Setpoint::velocity(Ned::new(1.0, 0.0, 0.0)),
            Setpoint::position(1.0, 2.0, 3.0),
        ] {
            let m = s.type_mask();
            assert_eq!(m & ALWAYS_IGNORED, ALWAYS_IGNORED, "{s:?}");
        }
    }

    #[test]
    fn every_setpoint_uses_the_relative_altitude_frame() {
        assert_eq!(
            Setpoint::velocity(Ned::ZERO).coordinate_frame(),
            MAV_FRAME_GLOBAL_RELATIVE_ALT_INT
        );
        assert_eq!(Setpoint::position(1.0, 2.0, 3.0).coordinate_frame(), 6);
    }

    #[test]
    fn a_velocity_command_saturates_at_the_speed_ceiling() {
        let s = Setpoint::velocity(Ned::new(300.0, 400.0, 0.0)); // magnitude 500
        let m = (s.vn as f64).hypot(s.ve as f64);
        assert!((m - MAX_COMMAND_SPEED_MPS).abs() < 1e-4, "{m}");
        // Direction preserved: 3-4-5 stays a 3-4-5.
        assert!((s.vn as f64 / m - 0.6).abs() < 1e-5);
    }

    #[test]
    fn ignored_position_fields_are_zeroed_not_left_stale() {
        let s = Setpoint::velocity(Ned::new(1.0, 1.0, 0.0));
        assert_eq!((s.lat_e7, s.lon_e7, s.alt_m), (0, 0, 0.0));
        let p = Setpoint::position(1.0, 2.0, 3.0);
        assert_eq!((p.vn, p.ve, p.vd), (0.0, 0.0, 0.0));
    }

    #[test]
    fn down_is_down_so_a_climb_is_negative_vd() {
        // A 1 m/s climb command.
        let s = Setpoint::velocity(Ned::new(0.0, 0.0, -1.0));
        assert!(s.vd < 0.0, "vd {} should be negative for a climb", s.vd);
    }

    #[test]
    fn position_at_round_trips_through_the_local_frame() {
        let o = GeoOrigin::new(12.9716, 77.5946, 100.0);
        let s = Setpoint::position_at(&o, Ned::new(50.0, -20.0, -10.0));
        let back = o.to_ned(
            crate::geo::e7_to_deg(s.lat_e7),
            crate::geo::e7_to_deg(s.lon_e7),
            s.alt_m as f64,
        );
        assert!(
            (back - Ned::new(50.0, -20.0, -10.0)).norm() < 0.02,
            "{back:?}"
        );
        assert!(
            (s.alt_m - 110.0).abs() < 1e-4,
            "10 m of negative down is 10 m up"
        );
    }

    #[test]
    fn a_non_finite_velocity_never_validates() {
        let bad = Setpoint {
            kind: SetpointKind::Velocity,
            lat_e7: 0,
            lon_e7: 0,
            alt_m: 0.0,
            vn: f32::NAN,
            ve: 0.0,
            vd: 0.0,
        };
        assert!(!bad.is_valid());
        assert!(Setpoint::velocity(Ned::new(1.0, 0.0, 0.0)).is_valid());
        // clamp_norm already scrubs a NaN command to zero magnitude, so the
        // constructor cannot produce an invalid setpoint.
        assert!(Setpoint::velocity(Ned::new(f64::NAN, 0.0, 0.0)).is_valid());
    }
}
