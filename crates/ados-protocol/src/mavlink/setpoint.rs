//! Guided position/velocity setpoints (SET_POSITION_TARGET_*).

use super::*;

/// MAVLink message id of `SET_POSITION_TARGET_LOCAL_NED`.
pub const MSG_ID_SET_POSITION_TARGET_LOCAL_NED: u32 = 84;

/// MAVLink message id of `SET_POSITION_TARGET_GLOBAL_INT`.
pub const MSG_ID_SET_POSITION_TARGET_GLOBAL_INT: u32 = 86;

/// Which of the two position-target messages a setpoint targets. The local
/// message carries x/y/z metres in a local/body frame; the global message
/// carries scaled lat/lon (1e7) plus an altitude in metres in a global frame.
/// A single-shot send picks one and fills the fields the type mask does not
/// ignore.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetpointKind {
    /// `SET_POSITION_TARGET_LOCAL_NED` (id 84): positions in metres in a
    /// local/body coordinate frame.
    LocalNed,
    /// `SET_POSITION_TARGET_GLOBAL_INT` (id 86): lat/lon scaled by 1e7 and an
    /// altitude in metres in a global coordinate frame.
    GlobalInt,
}

/// A single guided-mode setpoint to send to the autopilot.
///
/// This is a transport-agnostic data value, NOT a controller: building one and
/// sending it is one fire-and-forget MAVLink frame. It owns no flight state,
/// mode, or schedule. The caller is responsible for the policy around it:
///
/// * The autopilot accepts these only while it is in its guided/offboard mode;
///   the caller must put the vehicle there and confirm it. Building a setpoint
///   does not change the mode and does not check it.
/// * A guided setpoint stream is a heartbeat: most autopilots brake (and then
///   fail-safe, e.g. RTL) roughly three seconds after the last setpoint, so the
///   caller must re-send at well above that rate (about 3 Hz or faster) to hold
///   a commanded velocity. One frame on its own decays.
///
/// The `*_value` numeric fields are interpreted only where the corresponding
/// `type_mask` ignore bit is clear; an ignored axis still serializes (as its
/// stored value) but the autopilot disregards it. The builder validates that
/// every field that the mask does NOT ignore is finite, so a NaN/inf can never
/// reach the wire on an active axis.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GuidedSetpoint {
    /// Which message (local vs global) to build.
    pub kind: SetpointKind,
    /// The coordinate frame (`MAV_FRAME_*`). Validated against the small set of
    /// frames that are meaningful for the chosen message kind.
    pub coordinate_frame: u8,
    /// The `type_mask` ignore bits (`POSITION_TARGET_TYPEMASK_*`). A set bit
    /// ignores that axis. The high bits above the defined field are rejected so
    /// a malformed mask cannot smuggle unknown semantics to the autopilot.
    pub type_mask: u16,
    /// Position: x/y/z metres for [`SetpointKind::LocalNed`]. For
    /// [`SetpointKind::GlobalInt`] the x field carries the latitude and y the
    /// longitude, both already scaled by 1e7 (the wire integer); z is the
    /// altitude in metres. The builder maps them to the correct wire fields.
    pub x: f64,
    pub y: f64,
    pub z: f64,
    /// Velocity m/s in the chosen frame.
    pub vx: f32,
    pub vy: f32,
    pub vz: f32,
    /// Acceleration / force m/s^2 (or N when the force bit is set) in the frame.
    pub afx: f32,
    pub afy: f32,
    pub afz: f32,
    /// Yaw setpoint (rad) and yaw rate (rad/s).
    pub yaw: f32,
    pub yaw_rate: f32,
}

/// A guided-setpoint validation failure. The message is a stable, human-readable
/// string a caller can surface or log; it is not localized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetpointError(pub String);

impl std::fmt::Display for SetpointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SetpointError {}

/// The full set of `type_mask` bits the position-target messages define. A mask
/// with any bit outside this set is rejected: only the X/Y/Z, VX/VY/VZ,
/// AX/AY/AZ, FORCE_SET, YAW, and YAW_RATE bits are meaningful, so an unknown
/// high bit means a malformed request rather than a new semantic.
pub(super) const POSITION_TARGET_TYPEMASK_KNOWN_BITS: u16 = 0x0FFF;

/// Ignore bit: x position. The local message reads x as a coordinate, the global
/// message reads it as the latitude, so the same bit governs both.
pub(super) const TYPEMASK_X_IGNORE: u16 = 1;
/// Ignore bit: y position (longitude on the global message).
pub(super) const TYPEMASK_Y_IGNORE: u16 = 2;
/// Ignore bit: z position (altitude on the global message).
pub(super) const TYPEMASK_Z_IGNORE: u16 = 4;
pub(super) const TYPEMASK_VX_IGNORE: u16 = 8;
pub(super) const TYPEMASK_VY_IGNORE: u16 = 16;
pub(super) const TYPEMASK_VZ_IGNORE: u16 = 32;
pub(super) const TYPEMASK_AX_IGNORE: u16 = 64;
pub(super) const TYPEMASK_AY_IGNORE: u16 = 128;
pub(super) const TYPEMASK_AZ_IGNORE: u16 = 256;
pub(super) const TYPEMASK_YAW_IGNORE: u16 = 1024;
pub(super) const TYPEMASK_YAW_RATE_IGNORE: u16 = 2048;

/// The coordinate frames valid on `SET_POSITION_TARGET_LOCAL_NED`: a local NED
/// frame, its offset variant, and the body frames. (`MAV_FRAME_LOCAL_NED` 1,
/// `MAV_FRAME_LOCAL_OFFSET_NED` 7, `MAV_FRAME_BODY_NED` 8,
/// `MAV_FRAME_BODY_OFFSET_NED` 9, `MAV_FRAME_BODY_FRD` 12.)
pub(super) const LOCAL_FRAMES: &[u8] = &[1, 7, 8, 9, 12];

/// The coordinate frames valid on `SET_POSITION_TARGET_GLOBAL_INT`: the global
/// frames (`MAV_FRAME_GLOBAL` 0, `MAV_FRAME_GLOBAL_RELATIVE_ALT` 3,
/// `MAV_FRAME_GLOBAL_INT` 5, `MAV_FRAME_GLOBAL_RELATIVE_ALT_INT` 6).
pub(super) const GLOBAL_FRAMES: &[u8] = &[0, 3, 5, 6];

impl GuidedSetpoint {
    /// Validate the setpoint, returning the first problem found. Checks:
    /// * the `type_mask` carries no bit outside the defined field, and
    /// * the coordinate frame is one of the frames meaningful for the message
    ///   kind (local frames for the local message, global frames for the global
    ///   message), and
    /// * every numeric field on an axis the mask does NOT ignore is finite.
    ///
    /// An ignored axis is not range-checked (the autopilot disregards it), so a
    /// pure-velocity setpoint with a NaN left in an ignored position field is
    /// accepted — but a NaN on an active axis is rejected.
    pub fn validate(&self) -> Result<(), SetpointError> {
        if self.type_mask & !POSITION_TARGET_TYPEMASK_KNOWN_BITS != 0 {
            return Err(SetpointError(format!(
                "type_mask 0x{:04X} sets a bit outside the defined position-target field",
                self.type_mask
            )));
        }

        let valid_frames = match self.kind {
            SetpointKind::LocalNed => LOCAL_FRAMES,
            SetpointKind::GlobalInt => GLOBAL_FRAMES,
        };
        if !valid_frames.contains(&self.coordinate_frame) {
            return Err(SetpointError(format!(
                "coordinate_frame {} is not valid for this setpoint kind",
                self.coordinate_frame
            )));
        }

        // Every axis the mask does NOT ignore must be finite. The position
        // fields are f64 (the global message scales lat/lon as integers, which
        // an f64 holds exactly); the velocity / accel / yaw fields are f32.
        let active = |ignore_bit: u16| self.type_mask & ignore_bit == 0;
        let check_f64 = |v: f64, name: &str| -> Result<(), SetpointError> {
            if v.is_finite() {
                Ok(())
            } else {
                Err(SetpointError(format!("{name} must be a finite number")))
            }
        };
        // Some position axes narrow to f32 on the wire (the local message's
        // x/y/z and the global message's altitude). A finite f64 whose
        // magnitude exceeds f32::MAX would narrow to f32::INFINITY, defeating
        // the finiteness invariant on the wire, so such an axis is rejected
        // here when it is active. (Axes the mask ignores tolerate any value;
        // the wire field is disregarded by the autopilot.)
        let check_f64_narrows_to_f32 = |v: f64, name: &str| -> Result<(), SetpointError> {
            if v.is_finite() && (v as f32).is_finite() {
                Ok(())
            } else {
                Err(SetpointError(format!("{name} must be a finite number")))
            }
        };
        let check_f32 = |v: f32, name: &str| -> Result<(), SetpointError> {
            if v.is_finite() {
                Ok(())
            } else {
                Err(SetpointError(format!("{name} must be a finite number")))
            }
        };

        // x/y on the local message narrow to f32; on the global message they
        // carry scaled lat/lon clamped into i32 (a finite f64 saturates safely),
        // so the over-f32-MAX rejection applies only to the local kind.
        let position_narrows_to_f32 = matches!(self.kind, SetpointKind::LocalNed);
        if active(TYPEMASK_X_IGNORE) {
            if position_narrows_to_f32 {
                check_f64_narrows_to_f32(self.x, "x")?;
            } else {
                check_f64(self.x, "x")?;
            }
        }
        if active(TYPEMASK_Y_IGNORE) {
            if position_narrows_to_f32 {
                check_f64_narrows_to_f32(self.y, "y")?;
            } else {
                check_f64(self.y, "y")?;
            }
        }
        if active(TYPEMASK_Z_IGNORE) {
            // z narrows to f32 on both kinds (local z metres, global altitude).
            check_f64_narrows_to_f32(self.z, "z")?;
        }
        if active(TYPEMASK_VX_IGNORE) {
            check_f32(self.vx, "vx")?;
        }
        if active(TYPEMASK_VY_IGNORE) {
            check_f32(self.vy, "vy")?;
        }
        if active(TYPEMASK_VZ_IGNORE) {
            check_f32(self.vz, "vz")?;
        }
        if active(TYPEMASK_AX_IGNORE) {
            check_f32(self.afx, "afx")?;
        }
        if active(TYPEMASK_AY_IGNORE) {
            check_f32(self.afy, "afy")?;
        }
        if active(TYPEMASK_AZ_IGNORE) {
            check_f32(self.afz, "afz")?;
        }
        if active(TYPEMASK_YAW_IGNORE) {
            check_f32(self.yaw, "yaw")?;
        }
        if active(TYPEMASK_YAW_RATE_IGNORE) {
            check_f32(self.yaw_rate, "yaw_rate")?;
        }
        Ok(())
    }

    /// Validate, then build the typed [`MavMessage`] for this setpoint. The
    /// `target_system` / `target_component` address the vehicle; the global
    /// integer fields are clamped from the f64 inputs into i32 range so an
    /// out-of-range coordinate saturates rather than wrapping. The
    /// `time_boot_ms` is fixed at 0 (the autopilot does not require a monotonic
    /// stamp on a setpoint; ArduPilot ignores it on the inbound path).
    pub fn build_message(
        &self,
        target_system: u8,
        target_component: u8,
    ) -> Result<MavMessage, SetpointError> {
        self.validate()?;
        let type_mask = ardupilotmega::PositionTargetTypemask::from_bits_truncate(self.type_mask);
        Ok(match self.kind {
            SetpointKind::LocalNed => MavMessage::SET_POSITION_TARGET_LOCAL_NED(
                ardupilotmega::SET_POSITION_TARGET_LOCAL_NED_DATA {
                    time_boot_ms: 0,
                    x: self.x as f32,
                    y: self.y as f32,
                    z: self.z as f32,
                    vx: self.vx,
                    vy: self.vy,
                    vz: self.vz,
                    afx: self.afx,
                    afy: self.afy,
                    afz: self.afz,
                    yaw: self.yaw,
                    yaw_rate: self.yaw_rate,
                    type_mask,
                    target_system,
                    target_component,
                    coordinate_frame: mav_frame_from_u8(self.coordinate_frame),
                },
            ),
            SetpointKind::GlobalInt => {
                MavMessage::SET_POSITION_TARGET_GLOBAL_INT(
                    ardupilotmega::SET_POSITION_TARGET_GLOBAL_INT_DATA {
                        time_boot_ms: 0,
                        // x carries scaled latitude, y scaled longitude (already
                        // multiplied by 1e7 by the caller); clamp into i32 so an
                        // out-of-range value saturates rather than wraps.
                        lat_int: clamp_to_i32(self.x),
                        lon_int: clamp_to_i32(self.y),
                        alt: self.z as f32,
                        vx: self.vx,
                        vy: self.vy,
                        vz: self.vz,
                        afx: self.afx,
                        afy: self.afy,
                        afz: self.afz,
                        yaw: self.yaw,
                        yaw_rate: self.yaw_rate,
                        type_mask,
                        target_system,
                        target_component,
                        coordinate_frame: mav_frame_from_u8(self.coordinate_frame),
                    },
                )
            }
        })
    }
}
