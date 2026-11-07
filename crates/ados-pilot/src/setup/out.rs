//! What the setup engine asks of the outside world: MAVLink frames for the
//! FC and effects for the pilot (authority, agent config).

use ados_protocol::mavlink::ardupilotmega::{
    MavCmd, MavFrame, MavMessage, PositionTargetTypemask, COMMAND_INT_DATA, COMMAND_LONG_DATA,
    MANUAL_CONTROL_DATA, SET_POSITION_TARGET_LOCAL_NED_DATA,
};

/// Side effects outside MAVLink, applied by the pilot.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Owner becomes `tuning`: the pilot sends no setpoints of its own.
    EnterTuning,
    /// Leave `tuning`. `operator` = the operator took over, so the pilot
    /// holds as owner `pilot` instead of yielding to the FC.
    ExitTuning { operator: bool },
    /// Persist `control.mode` (`ados` | `fc_manual` | `elrs`).
    SetControlMode(&'static str),
    /// Persist `control.rc_ados_position` (flight-mode slot 1..6).
    SetRcAdosPosition(u8),
}

#[derive(Debug, Default)]
pub struct Outbox {
    pub msgs: Vec<MavMessage>,
    pub effects: Vec<Effect>,
    pub target: (u8, u8),
}

impl Outbox {
    pub fn new() -> Self {
        Outbox { target: (1, 1), ..Default::default() }
    }

    pub fn take_msgs(&mut self) -> Vec<MavMessage> {
        std::mem::take(&mut self.msgs)
    }

    pub fn take_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.effects)
    }

    pub fn effect(&mut self, e: Effect) {
        self.effects.push(e);
    }

    pub fn cmd(&mut self, command: MavCmd, p: [f32; 7]) {
        self.msgs.push(MavMessage::COMMAND_LONG(COMMAND_LONG_DATA {
            param1: p[0],
            param2: p[1],
            param3: p[2],
            param4: p[3],
            param5: p[4],
            param6: p[5],
            param7: p[6],
            command,
            target_system: self.target.0,
            target_component: self.target.1,
            confirmation: 0,
        }));
    }

    pub fn cmd_int(&mut self, command: MavCmd, frame: MavFrame, p: [f32; 4], x: i32, y: i32, z: f32) {
        self.msgs.push(MavMessage::COMMAND_INT(COMMAND_INT_DATA {
            param1: p[0],
            param2: p[1],
            param3: p[2],
            param4: p[3],
            x,
            y,
            z,
            command,
            target_system: self.target.0,
            target_component: self.target.1,
            frame,
            current: 0,
            autocontinue: 0,
        }));
    }

    /// DO_SET_MODE with a custom mode (ArduPilot: `main` is the mode number;
    /// PX4: main and sub mode).
    pub fn set_mode(&mut self, main: u32, sub: u32) {
        self.cmd(MavCmd::MAV_CMD_DO_SET_MODE, [1.0, main as f32, sub as f32, 0.0, 0.0, 0.0, 0.0]);
    }

    pub fn arm(&mut self, arm: bool) {
        self.cmd(MavCmd::MAV_CMD_COMPONENT_ARM_DISARM, [if arm { 1.0 } else { 0.0 }, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
    }

    pub fn takeoff(&mut self, alt_m: f32) {
        self.cmd(MavCmd::MAV_CMD_NAV_TAKEOFF, [0.0, 0.0, 0.0, f32::NAN, f32::NAN, f32::NAN, alt_m]);
    }

    pub fn message_interval(&mut self, msg_id: u32, hz: f32) {
        let us = if hz <= 0.0 { -1.0 } else { 1.0e6 / hz };
        self.cmd(MavCmd::MAV_CMD_SET_MESSAGE_INTERVAL, [msg_id as f32, us, 0.0, 0.0, 0.0, 0.0, 0.0]);
    }

    pub fn request_message(&mut self, msg_id: u32) {
        self.cmd(MavCmd::MAV_CMD_REQUEST_MESSAGE, [msg_id as f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
    }

    pub fn reboot(&mut self) {
        self.cmd(MavCmd::MAV_CMD_PREFLIGHT_REBOOT_SHUTDOWN, [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
    }

    /// MANUAL_CONTROL: `x` pitch, `y` roll, `r` yaw in -1..1; `z` throttle 0..1.
    pub fn manual(&mut self, x: f32, y: f32, z: f32, r: f32) {
        let s = |v: f32| (v.clamp(-1.0, 1.0) * 1000.0).round() as i16;
        self.msgs.push(MavMessage::MANUAL_CONTROL(MANUAL_CONTROL_DATA {
            x: s(x),
            y: s(y),
            z: (z.clamp(0.0, 1.0) * 1000.0).round() as i16,
            r: s(r),
            buttons: 0,
            target: self.target.0,
        }));
    }

    /// Position hold at `(n, e, d)` with yaw (rad), type mask 2496.
    pub fn hold_position(&mut self, n: f32, e: f32, d: f32, yaw: f32) {
        self.position_target(n, e, d, 0.0, 0.0, 0.0, yaw, 2496);
    }

    /// Velocity `(vn, ve, vd)` with yaw (rad), position ignored.
    pub fn velocity(&mut self, vn: f32, ve: f32, vd: f32, yaw: f32) {
        // Ignore position, acceleration and yaw rate: 0b0000_1001_1100_0111.
        self.position_target(0.0, 0.0, 0.0, vn, ve, vd, yaw, 0b0000_1001_1100_0111);
    }

    #[allow(clippy::too_many_arguments)]
    fn position_target(&mut self, x: f32, y: f32, z: f32, vx: f32, vy: f32, vz: f32, yaw: f32, mask: u16) {
        self.msgs.push(MavMessage::SET_POSITION_TARGET_LOCAL_NED(SET_POSITION_TARGET_LOCAL_NED_DATA {
            time_boot_ms: 0,
            x,
            y,
            z,
            vx,
            vy,
            vz,
            afx: 0.0,
            afy: 0.0,
            afz: 0.0,
            yaw,
            yaw_rate: 0.0,
            type_mask: PositionTargetTypemask::from_bits_truncate(mask),
            target_system: self.target.0,
            target_component: self.target.1,
            coordinate_frame: MavFrame::MAV_FRAME_LOCAL_NED,
        }));
    }
}
