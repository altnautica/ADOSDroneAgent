//! The FC's rate loop and motor outputs as the pilot reads them:
//! PID_TUNING (roll and pitch, with `GCS_PID_MASK` bits 0 and 1 set) and
//! SERVO_OUTPUT_RAW. The supervised lift-off loads the airframe's trim into
//! the rate integrators and watches them; airframe health learns the trim and
//! the motor headroom from them.

use ados_protocol::mavlink::ardupilotmega::{
    PidTuningAxis, PID_TUNING_DATA, SERVO_OUTPUT_RAW_DATA,
};

use super::Stamped;

/// One axis of the FC's rate controller (PID_TUNING): the rates as the FC
/// sends them (ArduPilot: deg/s), the terms in the controller's output units
/// (a fraction of the motor mix authority).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PidAxis {
    pub desired: f32,
    pub achieved: f32,
    pub ff: f32,
    pub p: f32,
    pub i: f32,
    pub d: f32,
}

/// Roll and pitch rate-loop state, `None` per axis until one arrives.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Rates {
    pub roll: Option<Stamped<PidAxis>>,
    pub pitch: Option<Stamped<PidAxis>>,
    /// SERVO_OUTPUT_RAW outputs 1..8 of port 0, µs.
    pub servo: Option<Stamped<[u16; 8]>>,
}

impl Rates {
    pub fn on_pid(&mut self, p: &PID_TUNING_DATA, now_ms: u64) {
        let v = PidAxis {
            desired: p.desired,
            achieved: p.achieved,
            ff: p.FF,
            p: p.P,
            i: p.I,
            d: p.D,
        };
        let s = Some(Stamped { v, at_ms: now_ms });
        match p.axis {
            PidTuningAxis::PID_TUNING_ROLL => self.roll = s,
            PidTuningAxis::PID_TUNING_PITCH => self.pitch = s,
            _ => {}
        }
    }

    pub fn on_servo(&mut self, s: &SERVO_OUTPUT_RAW_DATA, now_ms: u64) {
        if s.port != 0 {
            return;
        }
        let v = [
            s.servo1_raw,
            s.servo2_raw,
            s.servo3_raw,
            s.servo4_raw,
            s.servo5_raw,
            s.servo6_raw,
            s.servo7_raw,
            s.servo8_raw,
        ];
        self.servo = Some(Stamped { v, at_ms: now_ms });
    }

    /// The roll and pitch integrators while both are younger than
    /// `max_age_ms`.
    pub fn fresh_i(&self, now_ms: u64, max_age_ms: u64) -> Option<[f32; 2]> {
        let fresh = |a: Option<Stamped<PidAxis>>| {
            a.filter(|s| now_ms.saturating_sub(s.at_ms) < max_age_ms)
                .map(|s| s.v.i)
        };
        Some([fresh(self.roll)?, fresh(self.pitch)?])
    }

    /// The first `n` motor outputs while younger than `max_age_ms`.
    pub fn fresh_motors(&self, n: usize, now_ms: u64, max_age_ms: u64) -> Option<&[u16]> {
        self.servo
            .as_ref()
            .filter(|s| now_ms.saturating_sub(s.at_ms) < max_age_ms)
            .map(|s| &s.v[..n.min(8)])
    }
}
