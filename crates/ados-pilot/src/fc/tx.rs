//! The frames the pilot sends to the FC.

use ados_protocol::mavlink::ardupilotmega::{
    MavAutopilot, MavCmd, MavFrame, MavMessage, MavModeFlag, MavParamType, MavState, MavType,
    PositionTargetTypemask, COMMAND_INT_DATA, COMMAND_LONG_DATA, HEARTBEAT_DATA,
    MANUAL_CONTROL_DATA, PARAM_REQUEST_READ_DATA, PARAM_SET_DATA,
    SET_POSITION_TARGET_LOCAL_NED_DATA,
};

/// Frames the pilot sends, addressed to the FC it learned.
#[derive(Debug, Clone, Copy)]
pub struct Tx {
    pub sysid: u8,
    pub compid: u8,
}

impl Tx {
    pub fn setpoint(
        &self,
        mask: u16,
        pos: [f32; 3],
        vel: [f32; 3],
        yaw: f32,
        yaw_rate: f32,
    ) -> MavMessage {
        self.setpoint_acc(mask, pos, vel, [0.0; 3], yaw, yaw_rate)
    }

    /// [`Tx::setpoint`] with an acceleration (used when `mask` does not
    /// ignore it).
    pub fn setpoint_acc(
        &self,
        mask: u16,
        pos: [f32; 3],
        vel: [f32; 3],
        acc: [f32; 3],
        yaw: f32,
        yaw_rate: f32,
    ) -> MavMessage {
        MavMessage::SET_POSITION_TARGET_LOCAL_NED(SET_POSITION_TARGET_LOCAL_NED_DATA {
            time_boot_ms: 0,
            x: pos[0],
            y: pos[1],
            z: pos[2],
            vx: vel[0],
            vy: vel[1],
            vz: vel[2],
            afx: acc[0],
            afy: acc[1],
            afz: acc[2],
            yaw,
            yaw_rate,
            type_mask: PositionTargetTypemask::from_bits_truncate(mask),
            target_system: self.sysid,
            target_component: self.compid,
            coordinate_frame: MavFrame::MAV_FRAME_LOCAL_NED,
        })
    }

    pub fn command(&self, command: MavCmd, p: [f32; 7]) -> MavMessage {
        MavMessage::COMMAND_LONG(COMMAND_LONG_DATA {
            param1: p[0],
            param2: p[1],
            param3: p[2],
            param4: p[3],
            param5: p[4],
            param6: p[5],
            param7: p[6],
            command,
            target_system: self.sysid,
            target_component: self.compid,
            confirmation: 0,
        })
    }

    pub fn command_int(&self, command: MavCmd, p: [f32; 4], z: f32, frame: MavFrame) -> MavMessage {
        MavMessage::COMMAND_INT(COMMAND_INT_DATA {
            param1: p[0],
            param2: p[1],
            param3: p[2],
            param4: p[3],
            x: 0,
            y: 0,
            z,
            command,
            target_system: self.sysid,
            target_component: self.compid,
            frame,
            current: 0,
            autocontinue: 0,
        })
    }

    pub fn set_mode(&self, custom_mode: u32) -> MavMessage {
        let flag = MavModeFlag::MAV_MODE_FLAG_CUSTOM_MODE_ENABLED.bits() as f32;
        self.command(
            MavCmd::MAV_CMD_DO_SET_MODE,
            [flag, custom_mode as f32, 0.0, 0.0, 0.0, 0.0, 0.0],
        )
    }

    pub fn arm(&self, arm: bool, force: bool) -> MavMessage {
        let p2 = if force { 21196.0 } else { 0.0 };
        let p1 = if arm { 1.0 } else { 0.0 };
        self.command(
            MavCmd::MAV_CMD_COMPONENT_ARM_DISARM,
            [p1, p2, 0.0, 0.0, 0.0, 0.0, 0.0],
        )
    }

    pub fn manual_control(&self, axes: [f32; 4]) -> MavMessage {
        let s = |v: f32| (v.clamp(-1.0, 1.0) * 1000.0) as i16;
        MavMessage::MANUAL_CONTROL(MANUAL_CONTROL_DATA {
            x: s(axes[1]),
            y: s(axes[0]),
            z: ((axes[2].clamp(-1.0, 1.0) + 1.0) * 500.0) as i16,
            r: s(axes[3]),
            buttons: 0,
            target: self.sysid,
        })
    }

    pub fn param_request(&self, name: &str) -> MavMessage {
        MavMessage::PARAM_REQUEST_READ(PARAM_REQUEST_READ_DATA {
            param_index: -1,
            target_system: self.sysid,
            target_component: self.compid,
            param_id: param_id(name).into(),
        })
    }

    pub fn param_set(&self, name: &str, value: f32) -> MavMessage {
        MavMessage::PARAM_SET(PARAM_SET_DATA {
            param_value: value,
            target_system: self.sysid,
            target_component: self.compid,
            param_id: param_id(name).into(),
            param_type: MavParamType::MAV_PARAM_TYPE_REAL32,
        })
    }
}

fn param_id(name: &str) -> [u8; 16] {
    let mut id = [0u8; 16];
    let n = name.len().min(16);
    id[..n].copy_from_slice(&name.as_bytes()[..n]);
    id
}

/// The pilot's own 2 Hz heartbeat (sysid/compid stamped by the encoder).
pub fn heartbeat() -> MavMessage {
    MavMessage::HEARTBEAT(HEARTBEAT_DATA {
        custom_mode: 0,
        mavtype: MavType::MAV_TYPE_ONBOARD_CONTROLLER,
        autopilot: MavAutopilot::MAV_AUTOPILOT_INVALID,
        base_mode: MavModeFlag::empty(),
        system_status: MavState::MAV_STATE_ACTIVE,
        mavlink_version: 3,
    })
}
