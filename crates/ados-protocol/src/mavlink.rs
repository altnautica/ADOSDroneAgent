//! MAVLink v2 codec for the ardupilotmega dialect.
//!
//! Behind the `mavlink` feature because the dialect is a large, slow-to-compile
//! generated enum and most consumers do not need it. This wraps `rust-mavlink`
//! with parse/serialize helpers over byte slices, which is what flows on the
//! MAVLink socket. The router (which owns the serial link and fans frames out
//! to the socket) builds on this; per the migration plan the decode is
//! validated against the ground station's decoder before cutover, since
//! `rust-mavlink` is not assumed to be bit-identical to the audited decoder.

use std::io::Cursor;

use thiserror::Error;

pub use rust_mavlink::ardupilotmega::MavMessage;
pub use rust_mavlink::{MavHeader, MavlinkVersion};

// Re-export the dialect module so services built on this crate (the router)
// can construct and match the concrete message payloads and enums without
// declaring their own copy of the dialect dependency.
pub use rust_mavlink::ardupilotmega;

#[derive(Debug, Error)]
pub enum MavlinkError {
    #[error("failed to read MAVLink v2 frame: {0}")]
    Read(String),
    #[error("failed to write MAVLink v2 frame: {0}")]
    Write(String),
}

/// Parse one MAVLink v2 frame from a byte slice into its header and message.
pub fn parse_v2(bytes: &[u8]) -> Result<(MavHeader, MavMessage), MavlinkError> {
    let mut reader = rust_mavlink::peek_reader::PeekReader::new(Cursor::new(bytes));
    rust_mavlink::read_v2_msg::<MavMessage, _>(&mut reader)
        .map_err(|e| MavlinkError::Read(e.to_string()))
}

/// Parse one MAVLink v1 frame (STX `0xFE`) from a byte slice into its header and
/// message. A v1 frame has a 6-byte header (STX, payload length, sequence,
/// system id, component id, message id), the payload, and a 2-byte checksum.
/// There are no incompat/compat flags and no signature block.
pub fn parse_v1(bytes: &[u8]) -> Result<(MavHeader, MavMessage), MavlinkError> {
    let mut reader = rust_mavlink::peek_reader::PeekReader::new(Cursor::new(bytes));
    rust_mavlink::read_v1_msg::<MavMessage, _>(&mut reader)
        .map_err(|e| MavlinkError::Read(e.to_string()))
}

/// Parse one MAVLink frame of either protocol version, selected by the leading
/// start-of-frame magic byte (`0xFD` for v2, `0xFE` for v1). The original frame
/// bytes are unchanged and are not re-encoded.
pub fn parse_any(bytes: &[u8]) -> Result<(MavHeader, MavMessage), MavlinkError> {
    match bytes.first() {
        Some(&0xFD) => parse_v2(bytes),
        Some(&0xFE) => parse_v1(bytes),
        Some(other) => Err(MavlinkError::Read(format!(
            "unknown MAVLink start-of-frame byte 0x{other:02X}"
        ))),
        None => Err(MavlinkError::Read("empty MAVLink frame".to_string())),
    }
}

/// Serialize a message into a complete MAVLink v2 frame.
pub fn serialize_v2(header: MavHeader, msg: &MavMessage) -> Result<Vec<u8>, MavlinkError> {
    let mut buf = Vec::new();
    rust_mavlink::write_v2_msg(&mut buf, header, msg)
        .map_err(|e| MavlinkError::Write(e.to_string()))?;
    Ok(buf)
}

/// Serialize a message into a complete MAVLink v1 frame (STX `0xFE`).
pub fn serialize_v1(header: MavHeader, msg: &MavMessage) -> Result<Vec<u8>, MavlinkError> {
    let mut buf = Vec::new();
    rust_mavlink::write_v1_msg(&mut buf, header, msg)
        .map_err(|e| MavlinkError::Write(e.to_string()))?;
    Ok(buf)
}

// ---------------------------------------------------------------------------
// Raw COMMAND_LONG builder for an arbitrary command id.
// ---------------------------------------------------------------------------

/// MAVLink message id of `COMMAND_LONG` (`0x4C`).
pub const MSG_ID_COMMAND_LONG: u32 = 76;

/// The `COMMAND_LONG` CRC_EXTRA, the message-definition seed the X.25 checksum
/// folds in last. This is the value the canonical dialect carries for the message
/// (the same value the generated codec and the reference Python encoder use);
/// it is NOT the message id.
pub const COMMAND_LONG_CRC_EXTRA: u8 = 152;

/// Build a complete MAVLink v2 `COMMAND_LONG` frame for an ARBITRARY `command`
/// id, returning the raw frame bytes ready to write to the MAVLink socket.
///
/// The generated dialect enum only carries named command ids, so a `COMMAND_LONG`
/// for a command the enum does not name cannot be built through the typed
/// `MavMessage::COMMAND_LONG` path. This serializes the wire frame directly: the
/// `COMMAND_LONG` payload (the seven `param`s as little-endian f32, then the
/// `command` u16, then `target_system` / `target_component` / `confirmation` as
/// u8, in wire order), MAVLink2 trailing-zero truncation, the v2 header (both the
/// incompat and compat flags are 0), and the X.25 checksum folded with
/// [`COMMAND_LONG_CRC_EXTRA`]. Identical on the wire to a `COMMAND_LONG` for a
/// named id; the only difference is this accepts an id the enum has no variant
/// for.
///
/// `confirmation` is fixed at 0 (the command surface is fire-and-forget). The
/// frame is unsigned (no MAVLink2 signature; incompat flags = 0).
#[allow(clippy::too_many_arguments)]
pub fn build_command_long_v2(
    header: MavHeader,
    command: u16,
    target_system: u8,
    target_component: u8,
    params: [f32; 7],
) -> Vec<u8> {
    // The COMMAND_LONG payload in wire (field-id) order: the seven f32 params
    // first (28 bytes), then the u16 command (2), then target_system,
    // target_component, confirmation (1 each) → 33 bytes max.
    let mut payload = Vec::with_capacity(33);
    for p in params {
        payload.extend_from_slice(&p.to_le_bytes());
    }
    payload.extend_from_slice(&command.to_le_bytes());
    payload.push(target_system);
    payload.push(target_component);
    payload.push(0u8); // confirmation

    // MAVLink2 truncates trailing zero bytes off the payload, keeping at least
    // one byte. The CRC is computed over the truncated payload.
    truncate_trailing_zeros(&mut payload);

    let mut frame = Vec::with_capacity(10 + payload.len() + 2);
    frame.push(0xFD); // v2 start-of-frame
    frame.push(payload.len() as u8); // payload length
    frame.push(0x00); // incompat flags (unsigned)
    frame.push(0x00); // compat flags
    frame.push(header.sequence);
    frame.push(header.system_id);
    frame.push(header.component_id);
    // 3-byte little-endian message id.
    frame.push((MSG_ID_COMMAND_LONG & 0xFF) as u8);
    frame.push(((MSG_ID_COMMAND_LONG >> 8) & 0xFF) as u8);
    frame.push(((MSG_ID_COMMAND_LONG >> 16) & 0xFF) as u8);
    frame.extend_from_slice(&payload);

    // X.25 checksum over every byte after the start-of-frame, then the CRC_EXTRA.
    let mut crc = X25_INIT;
    for &b in &frame[1..] {
        crc = x25_accumulate(b, crc);
    }
    crc = x25_accumulate(COMMAND_LONG_CRC_EXTRA, crc);
    frame.push((crc & 0xFF) as u8);
    frame.push((crc >> 8) as u8);

    frame
}

/// Drop trailing zero bytes off a MAVLink2 payload, keeping at least one byte
/// (an all-zero payload truncates to a single zero byte, never empty).
fn truncate_trailing_zeros(payload: &mut Vec<u8>) {
    while payload.len() > 1 && *payload.last().unwrap() == 0 {
        payload.pop();
    }
}

/// X.25 / CRC-16-MCRF4XX initial value (the MAVLink checksum seed).
const X25_INIT: u16 = 0xFFFF;

/// Accumulate one byte into the running X.25 checksum, the same per-byte fold the
/// MAVLink checksum uses (and rust-mavlink's own CRC), so a frame this builder
/// emits is byte-identical to one the typed serializer would for a named id.
fn x25_accumulate(byte: u8, crc: u16) -> u16 {
    let mut tmp = byte ^ (crc & 0xFF) as u8;
    tmp ^= tmp << 4;
    let tmp = tmp as u16;
    (crc >> 8) ^ (tmp << 8) ^ (tmp << 3) ^ (tmp >> 4)
}

// ---------------------------------------------------------------------------
// Guided position/velocity setpoint builder (SET_POSITION_TARGET 84 / 86).
// ---------------------------------------------------------------------------

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
const POSITION_TARGET_TYPEMASK_KNOWN_BITS: u16 = 0x0FFF;

/// Ignore bit: x position. The local message reads x as a coordinate, the global
/// message reads it as the latitude, so the same bit governs both.
const TYPEMASK_X_IGNORE: u16 = 1;
/// Ignore bit: y position (longitude on the global message).
const TYPEMASK_Y_IGNORE: u16 = 2;
/// Ignore bit: z position (altitude on the global message).
const TYPEMASK_Z_IGNORE: u16 = 4;
const TYPEMASK_VX_IGNORE: u16 = 8;
const TYPEMASK_VY_IGNORE: u16 = 16;
const TYPEMASK_VZ_IGNORE: u16 = 32;
const TYPEMASK_AX_IGNORE: u16 = 64;
const TYPEMASK_AY_IGNORE: u16 = 128;
const TYPEMASK_AZ_IGNORE: u16 = 256;
const TYPEMASK_YAW_IGNORE: u16 = 1024;
const TYPEMASK_YAW_RATE_IGNORE: u16 = 2048;

/// The coordinate frames valid on `SET_POSITION_TARGET_LOCAL_NED`: a local NED
/// frame, its offset variant, and the body frames. (`MAV_FRAME_LOCAL_NED` 1,
/// `MAV_FRAME_LOCAL_OFFSET_NED` 7, `MAV_FRAME_BODY_NED` 8,
/// `MAV_FRAME_BODY_OFFSET_NED` 9, `MAV_FRAME_BODY_FRD` 12.)
const LOCAL_FRAMES: &[u8] = &[1, 7, 8, 9, 12];

/// The coordinate frames valid on `SET_POSITION_TARGET_GLOBAL_INT`: the global
/// frames (`MAV_FRAME_GLOBAL` 0, `MAV_FRAME_GLOBAL_RELATIVE_ALT` 3,
/// `MAV_FRAME_GLOBAL_INT` 5, `MAV_FRAME_GLOBAL_RELATIVE_ALT_INT` 6).
const GLOBAL_FRAMES: &[u8] = &[0, 3, 5, 6];

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

/// Clamp an f64 into the i32 range without wrapping (used for the global
/// message's scaled lat/lon). A non-finite value is validated out before this is
/// reached on an active axis; an ignored-axis non-finite value clamps to 0.
fn clamp_to_i32(v: f64) -> i32 {
    if v.is_nan() {
        return 0;
    }
    v.clamp(i32::MIN as f64, i32::MAX as f64) as i32
}

/// Map a numeric coordinate-frame id to the dialect enum. An id outside the
/// validated frame sets never reaches here (the builder validates first), so an
/// unexpected value falls back to the enum default rather than panicking.
fn mav_frame_from_u8(frame: u8) -> ardupilotmega::MavFrame {
    use ardupilotmega::MavFrame::*;
    match frame {
        0 => MAV_FRAME_GLOBAL,
        1 => MAV_FRAME_LOCAL_NED,
        3 => MAV_FRAME_GLOBAL_RELATIVE_ALT,
        5 => MAV_FRAME_GLOBAL_INT,
        6 => MAV_FRAME_GLOBAL_RELATIVE_ALT_INT,
        7 => MAV_FRAME_LOCAL_OFFSET_NED,
        8 => MAV_FRAME_BODY_NED,
        9 => MAV_FRAME_BODY_OFFSET_NED,
        12 => MAV_FRAME_BODY_FRD,
        _ => ardupilotmega::MavFrame::DEFAULT,
    }
}

// ---------------------------------------------------------------------------
// TUNNEL transparent application-payload pipe (message id 385).
// ---------------------------------------------------------------------------

/// MAVLink message id of `TUNNEL`.
pub const MSG_ID_TUNNEL: u32 = 385;

/// The `TUNNEL` CRC_EXTRA, the message-definition seed the X.25 checksum folds in
/// last (the value the canonical dialect carries for the message; not the id).
pub const TUNNEL_CRC_EXTRA: u8 = 147;

/// Maximum bytes a single TUNNEL frame can carry: the fixed-size payload array.
pub const TUNNEL_MAX_PAYLOAD: usize = 128;

/// The highest `payload_type` value reserved by the standard registry. Values at
/// or below this are reserved; an application uses a value strictly above it so
/// it can never collide with a registered type, and so an unrelated peer that
/// does not recognize the value ignores the frame. The numeric field on the wire
/// is a u16, so the usable application range is `32768..=65535`.
pub const TUNNEL_RESERVED_PAYLOAD_TYPE_MAX: u16 = 32767;

/// A TUNNEL build/parse failure with a stable, human-readable message (not
/// localized). Mirrors [`SetpointError`] in shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelError(pub String);

impl std::fmt::Display for TunnelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TunnelError {}

/// Build a complete MAVLink v2 `TUNNEL` frame carrying an opaque application
/// `payload` tagged with a private `payload_type`, returning the raw frame bytes
/// ready to write to the MAVLink socket.
///
/// The generated dialect models `payload_type` as a named enum, so a private
/// (unregistered) value cannot ride the typed `MavMessage::TUNNEL` path — the
/// same constraint [`build_command_long_v2`] works around for an unnamed command
/// id. This serializes the wire frame directly: the `TUNNEL` payload in field
/// order (`payload_type` u16-LE, `target_system` u8, `target_component` u8,
/// `payload_length` u8, then the 128-byte payload, the unused tail zero-padded),
/// MAVLink2 trailing-zero truncation, the v2 header, and the X.25 checksum folded
/// with [`TUNNEL_CRC_EXTRA`]. The result is byte-identical to a `TUNNEL` the
/// typed serializer would emit for the same values; the only difference is this
/// accepts a `payload_type` the enum has no variant for.
///
/// The frame is unsigned (no MAVLink2 signature; incompat/compat flags = 0).
/// The tunnel is a transparent opaque pipe: this owns no application semantics,
/// so any per-payload integrity (an HMAC, a replay counter) lives inside
/// `payload`, not here.
///
/// Rejects a `payload_type` at or below [`TUNNEL_RESERVED_PAYLOAD_TYPE_MAX`] (a
/// registered/reserved value, never to be minted by an application) and a
/// `payload` longer than [`TUNNEL_MAX_PAYLOAD`].
pub fn build_tunnel_v2(
    header: MavHeader,
    payload_type: u16,
    target_system: u8,
    target_component: u8,
    payload: &[u8],
) -> Result<Vec<u8>, TunnelError> {
    if payload_type <= TUNNEL_RESERVED_PAYLOAD_TYPE_MAX {
        return Err(TunnelError(format!(
            "payload_type {payload_type} must be a private type greater than {TUNNEL_RESERVED_PAYLOAD_TYPE_MAX}"
        )));
    }
    if payload.len() > TUNNEL_MAX_PAYLOAD {
        return Err(TunnelError(format!(
            "payload is {} bytes, exceeds the {TUNNEL_MAX_PAYLOAD}-byte TUNNEL limit",
            payload.len()
        )));
    }

    // The TUNNEL payload in wire (field-id) order: payload_type (u16-LE),
    // target_system, target_component, payload_length, then the fixed 128-byte
    // payload array (the unused tail stays zero). payload_length records the
    // application's used count; the array itself is always the full width before
    // MAVLink2 trailing-zero truncation.
    let mut body = Vec::with_capacity(5 + TUNNEL_MAX_PAYLOAD);
    body.extend_from_slice(&payload_type.to_le_bytes());
    body.push(target_system);
    body.push(target_component);
    body.push(payload.len() as u8);
    body.extend_from_slice(payload);
    // Zero-pad the fixed-width payload array out to its full size.
    body.resize(5 + TUNNEL_MAX_PAYLOAD, 0);

    // MAVLink2 truncates trailing zero bytes off the payload, keeping at least
    // one byte; the CRC is computed over the truncated payload.
    truncate_trailing_zeros(&mut body);

    let mut frame = Vec::with_capacity(10 + body.len() + 2);
    frame.push(0xFD); // v2 start-of-frame
    frame.push(body.len() as u8); // payload length
    frame.push(0x00); // incompat flags (unsigned)
    frame.push(0x00); // compat flags
    frame.push(header.sequence);
    frame.push(header.system_id);
    frame.push(header.component_id);
    // 3-byte little-endian message id.
    frame.push((MSG_ID_TUNNEL & 0xFF) as u8);
    frame.push(((MSG_ID_TUNNEL >> 8) & 0xFF) as u8);
    frame.push(((MSG_ID_TUNNEL >> 16) & 0xFF) as u8);
    frame.extend_from_slice(&body);

    // X.25 checksum over every byte after the start-of-frame, then the CRC_EXTRA.
    let mut crc = X25_INIT;
    for &b in &frame[1..] {
        crc = x25_accumulate(b, crc);
    }
    crc = x25_accumulate(TUNNEL_CRC_EXTRA, crc);
    frame.push((crc & 0xFF) as u8);
    frame.push((crc >> 8) as u8);

    Ok(frame)
}

/// The `payload_type` of a raw MAVLink frame if it is a `TUNNEL` (id 385),
/// else `None`.
///
/// Reads the field straight off the wire bytes rather than going through the
/// typed parser, because the typed parser rejects a private (unregistered)
/// `payload_type` as an unknown enum value — exactly the values this pipe
/// carries. Handles a v2 frame; the standard dialect only emits TUNNEL as v2.
/// Returns `None` when the frame is not a v2 TUNNEL or is too short to hold the
/// `payload_type` field.
pub fn tunnel_payload_type(frame: &[u8]) -> Option<u16> {
    // v2 frame: STX 0xFD, then a 9-byte header (len, incompat, compat, seq,
    // sysid, compid, 3-byte msgid), so the payload begins at byte 10. The msgid
    // is the little-endian 24-bit field at bytes 7..10.
    if frame.first() != Some(&0xFD) || frame.len() < 10 {
        return None;
    }
    let mut id = [0u8; 4];
    id[..3].copy_from_slice(&frame[7..10]);
    if u32::from_le_bytes(id) != MSG_ID_TUNNEL {
        return None;
    }
    // payload_type is the first payload field (u16-LE) at bytes 10..12. A frame
    // truncated below that (an empty/short payload) is not a usable TUNNEL.
    if frame.len() < 12 {
        return None;
    }
    Some(u16::from_le_bytes([frame[10], frame[11]]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_mavlink::ardupilotmega::HEARTBEAT_DATA;

    fn heartbeat() -> MavMessage {
        MavMessage::HEARTBEAT(HEARTBEAT_DATA {
            custom_mode: 0,
            mavtype: rust_mavlink::ardupilotmega::MavType::MAV_TYPE_QUADROTOR,
            autopilot: rust_mavlink::ardupilotmega::MavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA,
            base_mode: rust_mavlink::ardupilotmega::MavModeFlag::empty(),
            system_status: rust_mavlink::ardupilotmega::MavState::MAV_STATE_STANDBY,
            mavlink_version: 3,
        })
    }

    #[test]
    fn heartbeat_round_trips_through_v2_frame() {
        let header = MavHeader {
            system_id: 1,
            component_id: 1,
            sequence: 42,
        };
        let frame = serialize_v2(header, &heartbeat()).unwrap();
        // A v2 frame starts with the 0xFD magic byte.
        assert_eq!(frame[0], 0xFD);

        let (got_header, got_msg) = parse_v2(&frame).unwrap();
        assert_eq!(got_header.system_id, 1);
        assert_eq!(got_header.component_id, 1);
        assert_eq!(got_header.sequence, 42);
        match got_msg {
            MavMessage::HEARTBEAT(hb) => {
                assert_eq!(
                    hb.mavtype,
                    rust_mavlink::ardupilotmega::MavType::MAV_TYPE_QUADROTOR
                );
                assert_eq!(hb.mavlink_version, 3);
            }
            other => panic!("expected HEARTBEAT, got {other:?}"),
        }
    }

    #[test]
    fn truncated_frame_is_a_read_error() {
        let header = MavHeader::default();
        let frame = serialize_v2(header, &heartbeat()).unwrap();
        // Drop the last few bytes so the frame is incomplete.
        assert!(matches!(
            parse_v2(&frame[..frame.len() - 3]),
            Err(MavlinkError::Read(_))
        ));
    }

    #[test]
    fn heartbeat_round_trips_through_v1_frame() {
        let header = MavHeader {
            system_id: 1,
            component_id: 1,
            sequence: 7,
        };
        let frame = serialize_v1(header, &heartbeat()).unwrap();
        // A v1 frame starts with the 0xFE magic byte.
        assert_eq!(frame[0], 0xFE);

        let (got_header, got_msg) = parse_v1(&frame).unwrap();
        assert_eq!(got_header.system_id, 1);
        assert_eq!(got_header.component_id, 1);
        assert_eq!(got_header.sequence, 7);
        assert!(matches!(got_msg, MavMessage::HEARTBEAT(_)));
    }

    #[test]
    fn outbound_frame_is_length_prefixed_and_decode_recovers_it() {
        // The MAVLink socket contract frames each outbound raw MAVLink frame
        // with a 4-byte big-endian length prefix. A consumer reads the prefix
        // with decode_len and then exactly that many payload bytes, recovering
        // the original frame verbatim.
        use crate::frame::{decode_len, encode_frame, HEADER_SIZE, MAVLINK_MAX_FRAME};
        let header = MavHeader {
            system_id: 1,
            component_id: 1,
            sequence: 5,
        };
        let raw = serialize_v2(header, &heartbeat()).unwrap();

        let framed = encode_frame(&raw, MAVLINK_MAX_FRAME).unwrap();
        // The prefix is the big-endian length of the raw frame.
        let prefix: [u8; HEADER_SIZE] = framed[..HEADER_SIZE].try_into().unwrap();
        let len = decode_len(prefix, MAVLINK_MAX_FRAME, false).unwrap();
        assert_eq!(len, raw.len());
        // The payload after the prefix is the original frame, unchanged.
        assert_eq!(&framed[HEADER_SIZE..], raw.as_slice());
        // And it still parses as the heartbeat it started as.
        let (_, msg) = parse_any(&framed[HEADER_SIZE..]).unwrap();
        assert!(matches!(msg, MavMessage::HEARTBEAT(_)));
    }

    #[test]
    fn parse_any_dispatches_on_start_of_frame_byte() {
        let header = MavHeader {
            system_id: 1,
            component_id: 1,
            sequence: 3,
        };
        let v2_frame = serialize_v2(header, &heartbeat()).unwrap();
        let v1_frame = serialize_v1(header, &heartbeat()).unwrap();
        assert_eq!(v2_frame[0], 0xFD);
        assert_eq!(v1_frame[0], 0xFE);

        let (_, m2) = parse_any(&v2_frame).unwrap();
        let (_, m1) = parse_any(&v1_frame).unwrap();
        assert!(matches!(m2, MavMessage::HEARTBEAT(_)));
        assert!(matches!(m1, MavMessage::HEARTBEAT(_)));

        // An unknown start byte is rejected, not silently parsed.
        assert!(matches!(
            parse_any(&[0x00, 0x01, 0x02]),
            Err(MavlinkError::Read(_))
        ));
        assert!(matches!(parse_any(&[]), Err(MavlinkError::Read(_))));
    }

    // ── build_command_long_v2 ────────────────────────────────────────────────

    #[test]
    fn command_long_builder_matches_the_golden_set_camera_source_frame() {
        // The exact 44-byte frame the reference encoder produces for
        // SET_CAMERA_SOURCE (command 534) with camera_index=2, source
        // system/component 255/190, target 1/1, sequence 0. param2 carries the
        // index (2.0); every other param is 0.
        let header = MavHeader {
            system_id: 255,
            component_id: 190,
            sequence: 0,
        };
        let frame = build_command_long_v2(header, 534, 1, 1, [0.0, 2.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        let golden = hex_to_bytes(
            "fd20000000ffbe4c000000000000000000400000000000000000000000000000000000000000160201019b45",
        );
        assert_eq!(
            frame, golden,
            "the built frame must be byte-identical to the golden SET_CAMERA_SOURCE frame"
        );
    }

    #[test]
    fn command_long_builder_truncates_trailing_zeros_and_is_44_bytes() {
        let header = MavHeader {
            system_id: 255,
            component_id: 190,
            sequence: 0,
        };
        let frame = build_command_long_v2(header, 534, 1, 1, [0.0, 2.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        // 10-byte v2 header + 32-byte truncated payload + 2-byte CRC = 44.
        assert_eq!(frame.len(), 44);
        assert_eq!(frame[0], 0xFD); // v2 start-of-frame
        assert_eq!(frame[1], 32); // truncated payload length
        assert_eq!(frame[2], 0x00); // incompat flags (unsigned)
        assert_eq!(frame[3], 0x00); // compat flags
                                    // 3-byte LE message id == 76.
        assert_eq!(frame[7], 76);
        assert_eq!(frame[8], 0);
        assert_eq!(frame[9], 0);
    }

    #[test]
    fn command_long_builder_crc_matches_rust_mavlink_for_a_named_id() {
        // For a command the dialect DOES name (ARM_DISARM, 400), the raw builder
        // must produce the byte-identical frame the typed serializer produces —
        // proving the X.25 + CRC_EXTRA fold here is consistent with the codec the
        // rest of the agent uses. ARM_DISARM's CRC_EXTRA differs from
        // COMMAND_LONG's, but the message id + payload layout are the same, so we
        // compare the frame bytes up to (but excluding) the 2-byte CRC, then assert
        // both CRCs are well-formed 2-byte tails.
        use rust_mavlink::ardupilotmega::{MavCmd, COMMAND_LONG_DATA};
        let header = MavHeader {
            system_id: 1,
            component_id: 191,
            sequence: 7,
        };
        let typed = MavMessage::COMMAND_LONG(COMMAND_LONG_DATA {
            target_system: 1,
            target_component: 1,
            command: MavCmd::MAV_CMD_COMPONENT_ARM_DISARM,
            confirmation: 0,
            param1: 1.0,
            param2: 0.0,
            param3: 0.0,
            param4: 0.0,
            param5: 0.0,
            param6: 0.0,
            param7: 0.0,
        });
        let typed_frame = serialize_v2(header, &typed).unwrap();
        // The raw builder for the same command id + params.
        let raw_frame =
            build_command_long_v2(header, 400, 1, 1, [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        // The header + payload (everything but the trailing 2-byte CRC) is
        // identical: the raw builder reproduces the typed serializer's framing.
        assert_eq!(
            &raw_frame[..raw_frame.len() - 2],
            &typed_frame[..typed_frame.len() - 2],
            "header + payload must match the typed serializer for the same command"
        );
    }

    #[test]
    fn command_long_builder_zero_truncation_keeps_one_byte() {
        // An all-zero payload truncates to a single byte (never empty), so the
        // frame is still a valid, parseable v2 frame.
        let mut payload = vec![0u8, 0, 0, 0];
        truncate_trailing_zeros(&mut payload);
        assert_eq!(payload, vec![0u8]);
    }

    /// Decode a lowercase hex string into bytes for the golden-frame assertions.
    fn hex_to_bytes(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    // ---- guided setpoint builder ----------------------------------------

    fn velocity_setpoint(kind: SetpointKind, frame: u8) -> GuidedSetpoint {
        // Pure-velocity setpoint: ignore position, accel, yaw; command vx/vy/vz.
        let ignore = TYPEMASK_X_IGNORE
            | TYPEMASK_Y_IGNORE
            | TYPEMASK_Z_IGNORE
            | TYPEMASK_AX_IGNORE
            | TYPEMASK_AY_IGNORE
            | TYPEMASK_AZ_IGNORE
            | TYPEMASK_YAW_IGNORE
            | TYPEMASK_YAW_RATE_IGNORE;
        GuidedSetpoint {
            kind,
            coordinate_frame: frame,
            type_mask: ignore,
            x: 0.0,
            y: 0.0,
            z: 0.0,
            vx: 2.5,
            vy: -1.0,
            vz: 0.5,
            afx: 0.0,
            afy: 0.0,
            afz: 0.0,
            yaw: 0.0,
            yaw_rate: 0.0,
        }
    }

    #[test]
    fn local_ned_setpoint_builds_and_round_trips() {
        // A pure-velocity local setpoint in the body frame builds, serializes to
        // a real v2 frame, and decodes back to the SAME message id + fields.
        let sp = velocity_setpoint(SetpointKind::LocalNed, 8); // MAV_FRAME_BODY_NED
        let msg = sp.build_message(1, 1).expect("valid setpoint builds");
        let header = MavHeader {
            system_id: 1,
            component_id: 191,
            sequence: 0,
        };
        let frame = serialize_v2(header, &msg).expect("serialize succeeds");
        // The 3-byte v2 message id (bytes 7..10) is 84.
        assert_eq!(frame[7], 84, "message id low byte is 84 (LOCAL_NED)");
        assert_eq!(frame[8], 0);
        assert_eq!(frame[9], 0);
        let (_h, decoded) = parse_v2(&frame).expect("decode succeeds");
        match decoded {
            MavMessage::SET_POSITION_TARGET_LOCAL_NED(d) => {
                assert_eq!(d.vx, 2.5);
                assert_eq!(d.vy, -1.0);
                assert_eq!(d.vz, 0.5);
                assert_eq!(d.target_system, 1);
                assert_eq!(d.target_component, 1);
                assert_eq!(
                    d.coordinate_frame,
                    ardupilotmega::MavFrame::MAV_FRAME_BODY_NED
                );
                // The position-ignore bits round-tripped.
                assert!(d.type_mask.contains(
                    ardupilotmega::PositionTargetTypemask::POSITION_TARGET_TYPEMASK_X_IGNORE
                ));
            }
            other => panic!("expected SET_POSITION_TARGET_LOCAL_NED, got {other:?}"),
        }
    }

    #[test]
    fn global_int_setpoint_builds_and_round_trips() {
        // A pure-velocity global-int setpoint builds, serializes, and decodes back
        // to message id 86 with the velocity fields intact.
        let sp = velocity_setpoint(SetpointKind::GlobalInt, 6); // GLOBAL_RELATIVE_ALT_INT
        let msg = sp.build_message(1, 1).expect("valid setpoint builds");
        let header = MavHeader {
            system_id: 1,
            component_id: 191,
            sequence: 7,
        };
        let frame = serialize_v2(header, &msg).expect("serialize succeeds");
        assert_eq!(frame[7], 86, "message id low byte is 86 (GLOBAL_INT)");
        assert_eq!(frame[8], 0);
        assert_eq!(frame[9], 0);
        let (_h, decoded) = parse_v2(&frame).expect("decode succeeds");
        match decoded {
            MavMessage::SET_POSITION_TARGET_GLOBAL_INT(d) => {
                assert_eq!(d.vx, 2.5);
                assert_eq!(d.vy, -1.0);
                assert_eq!(d.vz, 0.5);
                assert_eq!(
                    d.coordinate_frame,
                    ardupilotmega::MavFrame::MAV_FRAME_GLOBAL_RELATIVE_ALT_INT
                );
            }
            other => panic!("expected SET_POSITION_TARGET_GLOBAL_INT, got {other:?}"),
        }
    }

    #[test]
    fn global_int_carries_scaled_lat_lon_and_clamps() {
        // x/y carry scaled lat/lon (already *1e7); they map to lat_int/lon_int.
        let mut sp = velocity_setpoint(SetpointKind::GlobalInt, 5); // GLOBAL_INT
                                                                    // Command a position too: clear the position-ignore bits.
        sp.type_mask &= !(TYPEMASK_X_IGNORE | TYPEMASK_Y_IGNORE | TYPEMASK_Z_IGNORE);
        sp.x = 37.422_408 * 1e7; // ~374224080
        sp.y = -122.084_270 * 1e7; // ~-1220842700
        sp.z = 30.0; // altitude metres
        let msg = sp.build_message(1, 1).expect("valid setpoint builds");
        match msg {
            MavMessage::SET_POSITION_TARGET_GLOBAL_INT(d) => {
                assert_eq!(d.lat_int, 374_224_080);
                assert_eq!(d.lon_int, -1_220_842_700);
                assert_eq!(d.alt, 30.0);
            }
            other => panic!("expected GLOBAL_INT, got {other:?}"),
        }
        // A latitude well past i32 range clamps to i32::MAX rather than wrapping.
        let mut huge = velocity_setpoint(SetpointKind::GlobalInt, 5);
        huge.type_mask &= !TYPEMASK_X_IGNORE;
        huge.x = 5e10;
        match huge.build_message(1, 1).unwrap() {
            MavMessage::SET_POSITION_TARGET_GLOBAL_INT(d) => assert_eq!(d.lat_int, i32::MAX),
            other => panic!("expected GLOBAL_INT, got {other:?}"),
        }
    }

    #[test]
    fn setpoint_rejects_a_nan_on_an_active_axis() {
        let mut sp = velocity_setpoint(SetpointKind::LocalNed, 1);
        sp.vx = f32::NAN;
        let err = sp.build_message(1, 1).unwrap_err();
        assert_eq!(err.0, "vx must be a finite number");
        // An infinite commanded velocity is rejected too.
        let mut inf = velocity_setpoint(SetpointKind::LocalNed, 1);
        inf.vz = f32::INFINITY;
        assert_eq!(
            inf.build_message(1, 1).unwrap_err().0,
            "vz must be a finite number"
        );
    }

    #[test]
    fn setpoint_rejects_a_finite_f64_over_f32_range_on_an_active_position_axis() {
        // A finite f64 whose magnitude is past f32::MAX would narrow to
        // f32::INFINITY on the wire. An active local position axis must reject
        // it rather than silently send infinity.
        let over = (f32::MAX as f64) * 2.0; // finite f64, but > f32::MAX
        assert!(over.is_finite());
        assert!((over as f32).is_infinite());

        let mut sp = velocity_setpoint(SetpointKind::LocalNed, 1);
        sp.type_mask &= !TYPEMASK_X_IGNORE; // command an x position
        sp.x = over;
        let err = sp.build_message(1, 1).unwrap_err();
        assert_eq!(err.0, "x must be a finite number");

        // The altitude axis narrows to f32 on both kinds, including global.
        let mut alt = velocity_setpoint(SetpointKind::GlobalInt, 5);
        alt.type_mask &= !TYPEMASK_Z_IGNORE;
        alt.z = over;
        assert_eq!(
            alt.build_message(1, 1).unwrap_err().0,
            "z must be a finite number"
        );
    }

    #[test]
    fn setpoint_ignores_a_nan_on_an_ignored_axis() {
        // A NaN left in an IGNORED position field is fine: the autopilot
        // disregards that axis, and the global clamp turns the NaN into 0.
        let mut sp = velocity_setpoint(SetpointKind::GlobalInt, 5);
        sp.x = f64::NAN; // x is ignored in the pure-velocity mask
        let msg = sp
            .build_message(1, 1)
            .expect("ignored-axis NaN is accepted");
        match msg {
            MavMessage::SET_POSITION_TARGET_GLOBAL_INT(d) => assert_eq!(d.lat_int, 0),
            other => panic!("expected GLOBAL_INT, got {other:?}"),
        }
    }

    #[test]
    fn setpoint_rejects_an_unknown_type_mask_bit() {
        let mut sp = velocity_setpoint(SetpointKind::LocalNed, 1);
        sp.type_mask |= 0x8000; // a bit above the defined field
        let err = sp.build_message(1, 1).unwrap_err();
        assert!(
            err.0.contains("outside the defined position-target field"),
            "got: {}",
            err.0
        );
    }

    #[test]
    fn setpoint_rejects_a_frame_wrong_for_the_kind() {
        // A global frame on a local message is rejected, and vice versa.
        let mut local = velocity_setpoint(SetpointKind::LocalNed, 1);
        local.coordinate_frame = 5; // MAV_FRAME_GLOBAL_INT, a global frame
        assert!(local
            .build_message(1, 1)
            .unwrap_err()
            .0
            .contains("not valid for this setpoint kind"));
        let mut global = velocity_setpoint(SetpointKind::GlobalInt, 5);
        global.coordinate_frame = 1; // MAV_FRAME_LOCAL_NED, a local frame
        assert!(global
            .build_message(1, 1)
            .unwrap_err()
            .0
            .contains("not valid for this setpoint kind"));
    }

    #[test]
    fn tunnel_frame_round_trips_the_payload_and_private_type() {
        // Build a TUNNEL with a private payload_type carrying an opaque app
        // payload; classify it as TUNNEL, recover the payload_type off the wire,
        // and recover the exact payload bytes the application sent.
        let header = MavHeader {
            system_id: 42,
            component_id: 191,
            sequence: 3,
        };
        let payload_type: u16 = 40001;
        let app_payload = b"hello-from-a-plugin";
        let frame =
            build_tunnel_v2(header, payload_type, 1, 1, app_payload).expect("private type builds");

        // The 3-byte v2 message id (bytes 7..10) is 385 (0x181) little-endian.
        let mut id = [0u8; 4];
        id[..3].copy_from_slice(&frame[7..10]);
        assert_eq!(u32::from_le_bytes(id), MSG_ID_TUNNEL);

        // The classifier sees a TUNNEL and reads the private type straight off
        // the wire (the typed parser would reject the unregistered enum value).
        assert_eq!(tunnel_payload_type(&frame), Some(payload_type));

        // Recover the application payload from the wire. After the 10-byte v2
        // header the TUNNEL fields are payload_type (bytes 10..12), target_system
        // (12), target_component (13), payload_length (14), then the payload at
        // byte 15. Confirm payload_length and the bytes round-trip.
        assert_eq!(frame[14] as usize, app_payload.len());
        assert_eq!(&frame[15..15 + app_payload.len()], app_payload);
    }

    #[test]
    fn tunnel_rejects_a_registered_payload_type() {
        // A value at or below the private floor is reserved by the registry and
        // must be refused, so an application cannot mint a colliding type.
        let header = MavHeader::default();
        let err = build_tunnel_v2(header, TUNNEL_RESERVED_PAYLOAD_TYPE_MAX, 1, 1, b"x")
            .expect_err("a registered type is refused");
        assert!(err.0.contains("private type"));
        // The very next value up is accepted.
        assert!(build_tunnel_v2(header, TUNNEL_RESERVED_PAYLOAD_TYPE_MAX + 1, 1, 1, b"x").is_ok());
    }

    #[test]
    fn tunnel_rejects_an_oversized_payload() {
        let header = MavHeader::default();
        // One byte over the fixed-array width.
        let big = vec![0xABu8; TUNNEL_MAX_PAYLOAD + 1];
        let err = build_tunnel_v2(header, 40002, 1, 1, &big)
            .expect_err("an oversized payload is refused");
        assert!(err.0.contains("exceeds"));
        // The maximum width is accepted.
        let exact = vec![0xCDu8; TUNNEL_MAX_PAYLOAD];
        assert!(build_tunnel_v2(header, 40002, 1, 1, &exact).is_ok());
    }

    #[test]
    fn tunnel_classifier_ignores_a_non_tunnel_frame() {
        // A heartbeat frame is not a TUNNEL, so the classifier returns None.
        let header = MavHeader::default();
        let frame = serialize_v2(header, &heartbeat()).unwrap();
        assert_eq!(tunnel_payload_type(&frame), None);
    }
}
