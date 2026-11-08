//! `accel`: six-position accelerometer calibration.
//!
//! ArduPilot: MAV_CMD_PREFLIGHT_CALIBRATION p5 = 1; the FC asks for each pose
//! with COMMAND_LONG ACCELCAL_VEHICLE_POS (42429) and the operator's "Next
//! pose" answers with the same command and pose. PX4: p5 = 1 and the `[cal]`
//! STATUSTEXT protocol; PX4 detects each side itself.

use ados_protocol::mavlink::ardupilotmega::{MavCmd, MavMessage, MavResult};
use serde_json::{json, Map, Value};

use crate::setup::adapters::px4::{parse_cal, CalEvent};
use crate::setup::fcstate::Fw;
use crate::setup::schema::{obj, Action};
use crate::setup::step::{step_core, ActErr, Cx, Step, StepCore};

const POS_SUCCESS: u8 = 16;
const POS_FAILED: u8 = 17;
const TIMEOUT_MS: u64 = 300_000;

fn pose_key(pose: u8) -> Option<&'static str> {
    Some(match pose {
        1 => "level",
        2 => "left",
        3 => "right",
        4 => "nose_down",
        5 => "nose_up",
        6 => "back",
        _ => return None,
    })
}

/// PX4 side names onto the same images.
fn px4_side_key(side: &str) -> &'static str {
    match side {
        "down" => "level",
        "up" => "back",
        "left" => "left",
        "right" => "right",
        "front" => "nose_down",
        _ => "nose_up",
    }
}

#[derive(Default)]
pub struct Accel {
    core: StepCore,
    started_ms: Option<u64>,
    pose: Option<u8>,
    poses_done: u8,
    text_cursor: u64,
}

impl Accel {
    fn finish(&mut self, ok: bool) {
        if ok {
            self.core.done(obj([("poses", json!(6))]));
        } else {
            self.core.fail("setup.blocker.accel_failed");
        }
    }
}

impl Step for Accel {
    step_core!("accel");

    fn optional(&self) -> bool {
        true
    }

    fn begin(&mut self, cx: &mut Cx) {
        self.core = StepCore::default();
        self.started_ms = None;
        self.pose = None;
        self.poses_done = 0;
        if cx.adapter.fw == Fw::Px4 {
            self.core.body_variant = Some("px4");
        }
        self.core.image = Some("accel_level".into());
        self.core
            .wait_user(vec![Action::primary("start"), Action::secondary("skip")]);
    }

    fn on_msg(&mut self, msg: &MavMessage, _cx: &mut Cx) {
        if self.started_ms.is_none() {
            return;
        }
        if let MavMessage::COMMAND_LONG(c) = msg {
            if c.command != MavCmd::MAV_CMD_ACCELCAL_VEHICLE_POS {
                return;
            }
            let pos = c.param1 as u8;
            match pos {
                POS_SUCCESS => self.finish(true),
                POS_FAILED => self.finish(false),
                p => {
                    if let Some(k) = pose_key(p) {
                        if self.pose.is_some_and(|prev| prev != p) {
                            self.poses_done += 1;
                        }
                        self.pose = Some(p);
                        self.core.image = Some(format!("accel_{k}"));
                        self.core.set_live("pose", k);
                        self.core.progress = Some(self.poses_done as f32 / 6.0);
                        self.core.wait_user(vec![
                            Action::primary("next_pose"),
                            Action::secondary("cancel"),
                        ]);
                    }
                }
            }
        }
    }

    fn tick(&mut self, cx: &mut Cx) {
        let Some(started) = self.started_ms else {
            return;
        };
        if self.core.state.is_terminal() {
            return;
        }
        if cx
            .fc
            .ack_since(MavCmd::MAV_CMD_PREFLIGHT_CALIBRATION as u32, started)
            .is_some_and(|r| {
                !matches!(
                    r,
                    MavResult::MAV_RESULT_ACCEPTED | MavResult::MAV_RESULT_IN_PROGRESS
                )
            })
        {
            self.core.fail("setup.blocker.calibration_refused");
            return;
        }
        let (texts, cursor) = cx.fc.texts_after(self.text_cursor);
        self.text_cursor = cursor;
        for t in texts {
            let l = t.to_ascii_lowercase();
            if cx.adapter.fw == Fw::Px4 {
                match parse_cal(&t) {
                    Some(CalEvent::Pending(sides)) => {
                        self.core.set_live("pending", json!(sides));
                        self.core.progress = Some((6 - sides.len().min(6)) as f32 / 6.0);
                        if let Some(s) = sides.first() {
                            self.core.image = Some(format!("accel_{}", px4_side_key(s)));
                        }
                    }
                    Some(CalEvent::Detected(side)) => {
                        self.core.image = Some(format!("accel_{}", px4_side_key(&side)));
                        self.core.set_live("pose", px4_side_key(&side));
                    }
                    Some(CalEvent::Done) => self.finish(true),
                    Some(CalEvent::Failed(_)) => self.finish(false),
                    _ => {}
                }
            } else if l.contains("calibration successful") {
                self.finish(true);
            } else if l.contains("calibration failed") || l.contains("calibration cancelled") {
                self.finish(false);
            }
        }
        if !self.core.state.is_terminal() && cx.now.saturating_sub(started) > TIMEOUT_MS {
            self.core.fail("setup.blocker.timeout");
        }
    }

    fn action(&mut self, id: &str, _args: &Map<String, Value>, cx: &mut Cx) -> Result<(), ActErr> {
        match id {
            "start" => {
                if cx.fc.armed {
                    return Err("armed");
                }
                self.started_ms = Some(cx.now);
                self.text_cursor = cx.fc.text_seq;
                cx.out.cmd(
                    MavCmd::MAV_CMD_PREFLIGHT_CALIBRATION,
                    [0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0],
                );
                self.core.running();
                self.core.actions = vec![Action::secondary("cancel")];
                Ok(())
            }
            "next_pose" => {
                let pose = self.pose.ok_or("no_pose")?;
                cx.out.cmd(
                    MavCmd::MAV_CMD_ACCELCAL_VEHICLE_POS,
                    [pose as f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
                );
                self.core.running();
                self.core.actions = vec![Action::secondary("cancel")];
                Ok(())
            }
            "cancel" => {
                self.abort(cx);
                Ok(())
            }
            _ => Err("unknown_action"),
        }
    }

    fn abort(&mut self, cx: &mut Cx) {
        if self.started_ms.take().is_some() {
            // An all-zero PREFLIGHT_CALIBRATION cancels a running calibration.
            cx.out.cmd(MavCmd::MAV_CMD_PREFLIGHT_CALIBRATION, [0.0; 7]);
        }
        self.core.fail("setup.blocker.cancelled");
    }
}
