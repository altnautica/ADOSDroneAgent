//! The test world's FC under a supervised lift-off: it answers PARAM_SET,
//! flies SET_ATTITUDE_TARGET on the test airframe ([`crate::liftoff::sim`]),
//! streams PID_TUNING and SERVO_OUTPUT_RAW, and follows the climbing
//! position target until the launch leaves its take-off step.

use ados_protocol::mavlink::ardupilotmega::*;

use super::harness::{World, FC_COMP, FC_SYS};
use crate::flows::LaunchStep;
use crate::liftoff::sim::Airframe;

impl World {
    /// The airframe the world's FC flies a supervised lift-off on.
    pub fn with_airframe(mut self, af: Airframe) -> Self {
        self.af = af;
        self
    }

    /// Run until the launch leaves its take-off step (the supervised
    /// lift-off spooled, lifted and climbed), at most `max_ms`.
    pub fn fly_takeoff(&mut self, max_ms: u64) {
        let end = self.now + max_ms;
        while self.now < end {
            self.run(20);
            let taking_off = self.p.launch.as_ref().is_some_and(|l| {
                matches!(
                    l.step,
                    LaunchStep::Checks | LaunchStep::Arm | LaunchStep::Takeoff
                )
            });
            if !taking_off {
                return;
            }
        }
        panic!(
            "still taking off after {max_ms} ms: {:?}",
            self.p.state(self.now).sequence
        );
    }

    /// The FC's answer to a frame the pilot sent: parameter writes echo back,
    /// attitude and position targets drive the airframe.
    pub(super) fn apply_airframe(&mut self, m: &MavMessage) {
        match m {
            MavMessage::PARAM_SET(p) if self.answers_params => {
                self.inbox.push(MavMessage::PARAM_VALUE(PARAM_VALUE_DATA {
                    param_value: p.param_value,
                    param_count: 1,
                    param_index: 0,
                    param_id: p.param_id,
                    param_type: p.param_type,
                }));
            }
            MavMessage::SET_ATTITUDE_TARGET(t) => {
                let q = t.q;
                let roll = (2.0 * (q[0] * q[1] + q[2] * q[3]))
                    .atan2(1.0 - 2.0 * (q[1] * q[1] + q[2] * q[2]));
                let pitch = (2.0 * (q[0] * q[2] - q[3] * q[1])).clamp(-1.0, 1.0).asin();
                self.af.target = Some(([roll, pitch, self.yaw], t.thrust));
                self.af_engaged = true;
            }
            MavMessage::SET_POSITION_TARGET_LOCAL_NED(s) if self.af_engaged => {
                self.af.goal_h = Some(-s.z);
            }
            _ => {}
        }
    }

    /// Advance the airframe one tick and put its state on the world.
    pub(super) fn step_airframe(&mut self, dt_ms: u64) {
        self.af.armed = self.armed;
        if !self.af_engaged {
            return;
        }
        let taking_off = self
            .p
            .launch
            .as_ref()
            .is_some_and(|l| l.step == LaunchStep::Takeoff);
        if !taking_off && self.af.airborne() {
            // Handed on: the FC finishes its climb to the launch height (as
            // the scripted FC's own take-off did), and the world is the
            // test's again.
            if let Some(l) = &self.p.launch {
                self.pos[2] = -l.alt_m;
                self.agl = Some(l.alt_m);
            }
            self.vel = [0.0; 3];
            self.roll = 0.0;
            self.pitch = 0.0;
            self.af_engaged = false;
            return;
        }
        let n = dt_ms / 5;
        for _ in 0..n {
            self.af.step(0.005);
        }
        self.roll = self.af.att[0];
        self.pitch = self.af.att[1];
        self.pos[2] = -self.af.h;
        self.vel[2] = -self.af.vz;
        self.agl = Some(0.1 + self.af.h);
        self.landed = !self.af.airborne();
        if !self.armed {
            self.af.target = None;
        }
    }

    /// The rate-loop and motor streams, while the vehicle is armed.
    pub(super) fn feed_airframe(&mut self) {
        if !self.armed || !self.rates_stream {
            return;
        }
        let mut msgs = Vec::new();
        for (axis, i) in [
            (PidTuningAxis::PID_TUNING_ROLL, self.af.i[0]),
            (PidTuningAxis::PID_TUNING_PITCH, self.af.i[1]),
        ] {
            msgs.push(MavMessage::PID_TUNING(PID_TUNING_DATA {
                I: i,
                axis,
                ..Default::default()
            }));
        }
        let o = self.af.motors();
        msgs.push(MavMessage::SERVO_OUTPUT_RAW(SERVO_OUTPUT_RAW_DATA {
            servo1_raw: o[0],
            servo2_raw: o[1],
            servo3_raw: o[2],
            servo4_raw: o[3],
            ..Default::default()
        }));
        for m in msgs {
            self.p.on_fc(FC_SYS, FC_COMP, &m, self.now);
        }
    }
}
