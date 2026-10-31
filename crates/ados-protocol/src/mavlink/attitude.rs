//! Attitude and body-rate setpoints (SET_ATTITUDE_TARGET).

use super::*;

/// MAVLink message id of `SET_ATTITUDE_TARGET`.
pub const MSG_ID_SET_ATTITUDE_TARGET: u32 = 82;

/// The `SET_ATTITUDE_TARGET` CRC_EXTRA, the message-definition seed the X.25
/// checksum folds in last (the value the canonical dialect carries for the
/// message; not the id). This is a distinct constant for this message — do not
/// confuse it with the `CRC_EXTRA` of any other id.
pub const ATTITUDE_TARGET_CRC_EXTRA: u8 = 49;

// ATTITUDE_TARGET_TYPEMASK bits (the values the ardupilotmega dialect carries
// for the message's `type_mask` bitfield). A set bit ignores that axis, exactly
// like the position-target mask semantics — an ignored axis serializes its
// stored value but the autopilot disregards it.
pub(super) const ATTMASK_BODY_ROLL_RATE_IGNORE: u8 = 1;
pub(super) const ATTMASK_BODY_PITCH_RATE_IGNORE: u8 = 2;
pub(super) const ATTMASK_BODY_YAW_RATE_IGNORE: u8 = 4;
/// `THRUST_BODY_SET`: the (extension) body-frame thrust vector is set. This
/// lane never commands it; the bit is accepted as known solely so a mask that
/// carries it is not misread as malformed.
pub(super) const ATTMASK_THRUST_BODY_SET: u8 = 32;
pub(super) const ATTMASK_THROTTLE_IGNORE: u8 = 64;
/// `ATTITUDE_IGNORE`: an unset bit here means the quaternion `q` is a commanded
/// attitude; when set, only the body rates (and possibly thrust) are meaningful.
pub(super) const ATTMASK_ATTITUDE_IGNORE: u8 = 128;

/// The full set of `type_mask` bits the attitude-target message defines. A mask
/// with any bit outside this set is rejected: only the rate/attitude/thrust
/// bits are meaningful, so an unknown high bit means a malformed request rather
/// than a new semantic.
pub(super) const ATTITUDE_TARGET_TYPEMASK_KNOWN_BITS: u8 = ATTMASK_BODY_ROLL_RATE_IGNORE
    | ATTMASK_BODY_PITCH_RATE_IGNORE
    | ATTMASK_BODY_YAW_RATE_IGNORE
    | ATTMASK_THRUST_BODY_SET
    | ATTMASK_THROTTLE_IGNORE
    | ATTMASK_ATTITUDE_IGNORE;

/// How far from unit length a commanded quaternion may be and still pass
/// validation. `SET_ATTITUDE_TARGET` consumes `q` as a unit quaternion; a
/// caller that hands it an unnormalized `q` would let the autopilot interpolate
/// garbage attitude. This is a strict tolerance: a control law that saturates
/// or accumulates drift is caught here, on this side of the wire.
pub(super) const QUAT_NORM_TOLERANCE: f32 = 1e-3;

/// A single `SET_ATTITUDE_TARGET` payload, frame-independent.
///
/// This is a transport-agnostic data value, NOT a controller: building one and
/// sending it is one fire-and-forget MAVLink frame. It owns no flight state,
/// mode, or schedule — the caller owns the policy around it (the autopilot
/// accepts these only in its attitude/offboard mode, and a command stream is a
/// heartbeat the caller must re-send well above the decay rate). The builder
/// validates that every field the `type_mask` does NOT ignore is finite, that a
/// commanded attitude is a unit quaternion, and that a commanded thrust is
/// within the normalized range, so a NaN / out-of-range command can never reach
/// the wire on an active axis.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AttitudeSetpoint {
    /// The `type_mask` ignore bits (`ATTITUDE_TARGET_TYPEMASK_*`). A set bit
    /// ignores that axis. The high bits above the defined field are rejected so
    /// a malformed mask cannot smuggle unknown semantics to the autopilot.
    pub type_mask: u8,
    /// Attitude quaternion `(w, x, y, z)`, MAVLink order; zero rotation is
    /// `(1, 0, 0, 0)`. Meaningful only while `ATTITUDE_IGNORE` is clear.
    pub q: [f32; 4],
    /// Body roll rate, rad/s. Ignored when `BODY_ROLL_RATE_IGNORE` is set.
    pub body_roll_rate: f32,
    /// Body pitch rate, rad/s. Ignored when `BODY_PITCH_RATE_IGNORE` is set.
    pub body_pitch_rate: f32,
    /// Body yaw rate, rad/s. Ignored when `BODY_YAW_RATE_IGNORE` is set.
    pub body_yaw_rate: f32,
    /// Collective thrust, normalized `0..1` (`-1..1` for reverse-thrust
    /// vehicles). Ignored when `THROTTLE_IGNORE` is set.
    pub thrust: f32,
}

impl AttitudeSetpoint {
    /// Validate the setpoint, returning the first problem found. Mirrors
    /// [`GuidedSetpoint::validate`] so an out-of-range command is refused at
    /// construction:
    /// * the `type_mask` carries no bit outside the defined field, and
    /// * every numeric field on an axis the mask does NOT ignore is finite, and
    /// * a commanded attitude is a unit quaternion, and
    /// * a commanded thrust is within the normalized `-1..=1` range.
    ///
    /// An ignored axis is not checked (the autopilot disregards it), so a pure
    /// body-rate command with a NaN left in the (ignored) quaternion is
    /// accepted — but a NaN or out-of-range value on an active axis is rejected.
    pub fn validate(&self) -> Result<(), SetpointError> {
        if self.type_mask & !ATTITUDE_TARGET_TYPEMASK_KNOWN_BITS != 0 {
            return Err(SetpointError(format!(
                "type_mask 0x{:02X} sets a bit outside the defined attitude-target field",
                self.type_mask
            )));
        }

        let active = |bit: u8| self.type_mask & bit == 0;
        let check_f32 = |v: f32, name: &str| -> Result<(), SetpointError> {
            if v.is_finite() {
                Ok(())
            } else {
                Err(SetpointError(format!("{name} must be a finite number")))
            }
        };

        if active(ATTMASK_ATTITUDE_IGNORE) {
            for (i, c) in self.q.iter().enumerate() {
                check_f32(*c, &format!("q[{i}]"))?;
            }
            let norm = (self.q[0] * self.q[0]
                + self.q[1] * self.q[1]
                + self.q[2] * self.q[2]
                + self.q[3] * self.q[3])
                .sqrt();
            if (norm - 1.0).abs() > QUAT_NORM_TOLERANCE {
                return Err(SetpointError(format!(
                    "q must be a unit quaternion (norm {norm})"
                )));
            }
        }
        if active(ATTMASK_BODY_ROLL_RATE_IGNORE) {
            check_f32(self.body_roll_rate, "body_roll_rate")?;
        }
        if active(ATTMASK_BODY_PITCH_RATE_IGNORE) {
            check_f32(self.body_pitch_rate, "body_pitch_rate")?;
        }
        if active(ATTMASK_BODY_YAW_RATE_IGNORE) {
            check_f32(self.body_yaw_rate, "body_yaw_rate")?;
        }
        if active(ATTMASK_THROTTLE_IGNORE) {
            check_f32(self.thrust, "thrust")?;
            if !(-1.0..=1.0).contains(&self.thrust) {
                return Err(SetpointError(format!(
                    "thrust {} must be in -1..=1",
                    self.thrust
                )));
            }
        }
        Ok(())
    }

    /// Validate, then build the typed [`MavMessage`] for this setpoint. The
    /// `target_system` / `target_component` address the vehicle; `time_boot_ms`
    /// is fixed at 0 (the autopilot does not require a monotonic stamp on an
    /// attitude setpoint; ArduPilot ignores it on the inbound path). The actual
    /// X.25 CRC is folded with [`ATTITUDE_TARGET_CRC_EXTRA`] (49) by the typed
    /// serializer, identical to what this builder's own encoder would emit.
    pub fn build_message(
        &self,
        target_system: u8,
        target_component: u8,
    ) -> Result<MavMessage, SetpointError> {
        self.validate()?;
        let type_mask = ardupilotmega::AttitudeTargetTypemask::from_bits_truncate(self.type_mask);
        Ok(MavMessage::SET_ATTITUDE_TARGET(
            ardupilotmega::SET_ATTITUDE_TARGET_DATA {
                time_boot_ms: 0,
                q: self.q,
                body_roll_rate: self.body_roll_rate,
                body_pitch_rate: self.body_pitch_rate,
                body_yaw_rate: self.body_yaw_rate,
                thrust: self.thrust,
                target_system,
                target_component,
                type_mask,
            },
        ))
    }
}

// ---------------------------------------------------------------------------
// TUNNEL transparent application-payload pipe (message id 385).
// ---------------------------------------------------------------------------
