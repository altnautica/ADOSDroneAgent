//! Guided, rate and TUNNEL request parsing and command targeting.

use super::*;

// ---------------------------------------------------------------------
// Guided setpoint parse
// ---------------------------------------------------------------------

/// Source system id stamped on a guided-setpoint frame: the agent/companion
/// identity the router uses on its own FC send path, so a setpoint from this
/// surface is wire-identical to one the router sent. Matches the native command
/// surface's source identity.
pub(super) const GUIDED_SOURCE_SYSTEM_ID: u8 = 1;
pub(super) const GUIDED_SOURCE_COMPONENT_ID: u8 = 191;

/// The `(target_system, target_component)` a plugin command is addressed to:
/// each explicit arg wins, and an omitted one comes from `observed`, the
/// autopilot identity seen on the router link. With neither, the command is
/// refused: guessing `1/1` sends it to a system a fleet drone is not, and the
/// autopilot drops it while the plugin reads `sent: true`.
pub(super) fn command_target(
    args: &Value,
    observed: Option<(u8, u8)>,
) -> Result<(u8, u8), HostError> {
    let system = arg_int::<u8>(args, "target_system")?.or(observed.map(|o| o.0));
    let component = arg_int::<u8>(args, "target_component")?.or(observed.map(|o| o.1));
    match (system, component) {
        (Some(s), Some(c)) => Ok((s, c)),
        _ => Err(HostError::Rpc(
            "no flight controller identity observed yet; pass target_system and \
             target_component"
                .to_string(),
        )),
    }
}

/// Source identity stamped on a TUNNEL frame from this surface: the same
/// agent/companion identity the guided-setpoint surface and the router use, so a
/// TUNNEL frame from a plugin is wire-consistent with the agent's other sends.
pub(super) const TUNNEL_SOURCE_SYSTEM_ID: u8 = GUIDED_SOURCE_SYSTEM_ID;
pub(super) const TUNNEL_SOURCE_COMPONENT_ID: u8 = GUIDED_SOURCE_COMPONENT_ID;

/// The MAVLink message id of the message this setpoint builds, for the response.
pub(super) fn setpoint_msg_id(sp: &ados_protocol::mavlink::GuidedSetpoint) -> u32 {
    use ados_protocol::mavlink::SetpointKind;
    match sp.kind {
        SetpointKind::LocalNed => ados_protocol::mavlink::MSG_ID_SET_POSITION_TARGET_LOCAL_NED,
        SetpointKind::GlobalInt => ados_protocol::mavlink::MSG_ID_SET_POSITION_TARGET_GLOBAL_INT,
    }
}

/// Parse a `flight.guided_setpoint.send` request into a [`GuidedSetpoint`].
///
/// Required: `kind` (`"local_ned"` | `"global_int"`), `coordinate_frame` (an
/// integer `MAV_FRAME_*`), and `type_mask` (an integer that fits u16; a set bit
/// ignores that axis). The numeric axis fields default to 0 when absent (an
/// ignored axis is conventionally left unset); each is read as a number and a
/// present non-number is an error. The finiteness / sane-mask / valid-frame
/// checks are NOT applied here — they live in [`GuidedSetpoint::validate`],
/// called by `build_message`, so the policy lives in one place.
pub(super) fn parse_guided_setpoint(
    args: &Value,
) -> Result<ados_protocol::mavlink::GuidedSetpoint, HostError> {
    use ados_protocol::mavlink::{GuidedSetpoint, SetpointKind};

    let kind = match arg_str(args, "kind") {
        Some("local_ned") => SetpointKind::LocalNed,
        Some("global_int") => SetpointKind::GlobalInt,
        Some(other) => {
            return Err(HostError::Rpc(format!(
                "kind must be \"local_ned\" or \"global_int\", got {other:?}"
            )))
        }
        None => {
            return Err(HostError::Rpc(
                "kind must be \"local_ned\" or \"global_int\"".to_string(),
            ))
        }
    };

    let coordinate_frame = match arg_i64(args, "coordinate_frame") {
        Some(n) if (0..=u8::MAX as i64).contains(&n) => n as u8,
        Some(_) => return Err(HostError::Rpc("coordinate_frame out of range".to_string())),
        None => {
            return Err(HostError::Rpc(
                "coordinate_frame must be an integer".to_string(),
            ))
        }
    };

    let type_mask = match arg_i64(args, "type_mask") {
        Some(n) if (0..=u16::MAX as i64).contains(&n) => n as u16,
        Some(_) => return Err(HostError::Rpc("type_mask out of range".to_string())),
        None => return Err(HostError::Rpc("type_mask must be an integer".to_string())),
    };

    Ok(GuidedSetpoint {
        kind,
        coordinate_frame,
        type_mask,
        x: arg_f64(args, "x")?,
        y: arg_f64(args, "y")?,
        z: arg_f64(args, "z")?,
        vx: arg_f32(args, "vx")?,
        vy: arg_f32(args, "vy")?,
        vz: arg_f32(args, "vz")?,
        afx: arg_f32(args, "afx")?,
        afy: arg_f32(args, "afy")?,
        afz: arg_f32(args, "afz")?,
        yaw: arg_f32(args, "yaw")?,
        yaw_rate: arg_f32(args, "yaw_rate")?,
    })
}

/// Parse a `flight.rate_setpoint.send` request into an [`AttitudeSetpoint`].
///
/// Required: `type_mask` (an integer that fits u8; a set bit ignores that axis).
/// The attitude quaternion is read as `qw`/`qx`/`qy`/`qz` (MAVLink `(w,x,y,z)`
/// order) and the body rates as `body_roll_rate`/`body_pitch_rate`/
/// `body_yaw_rate`, all defaulting to 0 when absent (an ignored axis is
/// conventionally left unset); `thrust` likewise. The finiteness /
/// unit-quaternion / thrust-range checks are NOT applied here — they live in
/// [`AttitudeSetpoint::validate`], called by `build_message`, so the policy
/// lives in one place.
pub(super) fn parse_rate_setpoint(
    args: &Value,
) -> Result<ados_protocol::mavlink::AttitudeSetpoint, HostError> {
    let type_mask = match arg_i64(args, "type_mask") {
        Some(n) if (0..=u8::MAX as i64).contains(&n) => n as u8,
        Some(_) => return Err(HostError::Rpc("type_mask out of range".to_string())),
        None => return Err(HostError::Rpc("type_mask must be an integer".to_string())),
    };

    Ok(ados_protocol::mavlink::AttitudeSetpoint {
        type_mask,
        q: [
            arg_f32(args, "qw")?,
            arg_f32(args, "qx")?,
            arg_f32(args, "qy")?,
            arg_f32(args, "qz")?,
        ],
        body_roll_rate: arg_f32(args, "body_roll_rate")?,
        body_pitch_rate: arg_f32(args, "body_pitch_rate")?,
        body_yaw_rate: arg_f32(args, "body_yaw_rate")?,
        thrust: arg_f32(args, "thrust")?,
    })
}
