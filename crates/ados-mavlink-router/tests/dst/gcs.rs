//! A simulated ground station on the router's MAVLink proxy: a 1 Hz
//! HEARTBEAT, mission uploads with a ground station's own reply timeout and
//! retries, and (for the command gates) a stream of labelled commands.

use ados_protocol::mavlink::ardupilotmega::*;

use crate::fc::{self, waypoint};
use crate::mav;
use crate::rng::Rng;

pub const SYS: u8 = 255;
pub const COMP: u8 = 190;
/// How long the ground station waits for the FC's next request or ACK.
pub const REPLY_TIMEOUT_MS: u64 = 1_500;
/// Re-sends of one step before the upload is given up.
pub const TRIES: u8 = 5;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Outcome {
    Accepted,
    Rejected(MavMissionResult),
    TimedOut,
}

pub struct Upload {
    pub started: u64,
    pub items: Vec<MISSION_ITEM_INT_DATA>,
    pub outcome: Option<(u64, Outcome)>,
    last: Vec<u8>,
    last_at: u64,
    tries: u8,
}

impl Upload {
    /// The longest an upload of this size may take before it must have an
    /// outcome: every step waits out every retry.
    pub fn deadline(&self) -> u64 {
        self.started
            + (self.items.len() as u64 + 1) * (u64::from(TRIES) + 1) * REPLY_TIMEOUT_MS
            + 1_000
    }
}

pub struct Gcs {
    seq: u8,
    next_hb: u64,
    pub uploads: Vec<Upload>,
    out: Vec<Vec<u8>>,
}

impl Gcs {
    pub fn new() -> Self {
        Self {
            seq: 0,
            next_hb: 0,
            uploads: Vec::new(),
            out: Vec::new(),
        }
    }

    pub fn frame(&mut self, msg: &MavMessage) -> Vec<u8> {
        self.seq = self.seq.wrapping_add(1);
        mav::encode(SYS, COMP, self.seq, msg)
    }

    pub fn active(&self) -> bool {
        self.uploads.last().is_some_and(|u| u.outcome.is_none())
    }

    /// Start uploading `items` (no-op while an upload is under way).
    pub fn upload(&mut self, now: u64, items: Vec<MISSION_ITEM_INT_DATA>) {
        if self.active() {
            return;
        }
        let count = MavMessage::MISSION_COUNT(MISSION_COUNT_DATA {
            count: items.len() as u16,
            target_system: fc::SYS,
            target_component: fc::COMP,
        });
        let last = self.frame(&count);
        self.out.push(last.clone());
        self.uploads.push(Upload {
            started: now,
            items,
            outcome: None,
            last,
            last_at: now,
            tries: 0,
        });
    }

    /// Frames off the router's fan-out.
    pub fn on_bytes(&mut self, now: u64, bytes: &[u8]) {
        for (h, msg, _) in mav::decode(bytes) {
            if (h.system_id, h.component_id) != (fc::SYS, fc::COMP) {
                continue;
            }
            match msg {
                MavMessage::MISSION_REQUEST_INT(r)
                    if (r.target_system, r.target_component) == (SYS, COMP) =>
                {
                    self.send_item(now, r.seq);
                }
                MavMessage::MISSION_ACK(a)
                    if (a.target_system, a.target_component) == (SYS, COMP) =>
                {
                    if let Some(up) = self.uploads.last_mut().filter(|u| u.outcome.is_none()) {
                        let o = match a.mavtype {
                            MavMissionResult::MAV_MISSION_ACCEPTED => Outcome::Accepted,
                            r => Outcome::Rejected(r),
                        };
                        up.outcome = Some((now, o));
                    }
                }
                _ => {}
            }
        }
    }

    fn send_item(&mut self, now: u64, seq: u16) {
        let Some(item) = self
            .uploads
            .last()
            .filter(|u| u.outcome.is_none())
            .and_then(|u| u.items.get(usize::from(seq)).cloned())
        else {
            return;
        };
        let f = self.frame(&MavMessage::MISSION_ITEM_INT(MISSION_ITEM_INT_DATA {
            target_system: fc::SYS,
            target_component: fc::COMP,
            ..item
        }));
        self.out.push(f.clone());
        let up = self.uploads.last_mut().expect("active");
        up.last = f;
        up.last_at = now;
        up.tries = 0;
    }

    /// The frames the ground station sends this tick.
    pub fn tick(&mut self, now: u64) -> Vec<Vec<u8>> {
        if now >= self.next_hb {
            self.next_hb = now + 1_000;
            let hb = self.frame(&MavMessage::HEARTBEAT(HEARTBEAT_DATA {
                mavtype: MavType::MAV_TYPE_GCS,
                autopilot: MavAutopilot::MAV_AUTOPILOT_INVALID,
                mavlink_version: 3,
                ..Default::default()
            }));
            self.out.push(hb);
        }
        if let Some(up) = self.uploads.last_mut().filter(|u| u.outcome.is_none()) {
            if now - up.last_at >= REPLY_TIMEOUT_MS {
                up.tries += 1;
                up.last_at = now;
                if up.tries > TRIES {
                    up.outcome = Some((now, Outcome::TimedOut));
                } else {
                    let f = up.last.clone();
                    self.out.push(f);
                }
            }
        }
        std::mem::take(&mut self.out)
    }
}

/// What a labelled command is, for the gate oracles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Moves or reconfigures the vehicle: a visor drops these.
    Flight,
    /// Reads or announces: passes every gate but the command-down gate.
    Benign,
}

/// One random command frame from the ground station, with its label.
pub fn command(g: &mut Gcs, rng: &mut Rng) -> (Kind, Vec<u8>) {
    let long = |command, p1: f32, p2: f32| {
        MavMessage::COMMAND_LONG(COMMAND_LONG_DATA {
            target_system: fc::SYS,
            target_component: fc::COMP,
            command,
            param1: p1,
            param2: p2,
            ..Default::default()
        })
    };
    let (kind, msg) = match rng.between(0, 9) {
        0 => (
            Kind::Flight,
            long(MavCmd::MAV_CMD_COMPONENT_ARM_DISARM, 1.0, 0.0),
        ),
        1 => (Kind::Flight, long(MavCmd::MAV_CMD_DO_SET_MODE, 1.0, 6.0)),
        2 => (Kind::Flight, long(MavCmd::MAV_CMD_NAV_TAKEOFF, 0.0, 0.0)),
        3 => (
            Kind::Flight,
            MavMessage::MANUAL_CONTROL(MANUAL_CONTROL_DATA {
                target: fc::SYS,
                x: 300,
                z: 500,
                ..Default::default()
            }),
        ),
        4 => (
            Kind::Flight,
            MavMessage::RC_CHANNELS_OVERRIDE(RC_CHANNELS_OVERRIDE_DATA {
                target_system: fc::SYS,
                target_component: fc::COMP,
                chan3_raw: 1600,
                ..Default::default()
            }),
        ),
        5 => (
            Kind::Flight,
            MavMessage::MISSION_ITEM_INT(MISSION_ITEM_INT_DATA {
                target_system: fc::SYS,
                target_component: fc::COMP,
                ..waypoint(1, 473_977_000, 85_456_000, 20.0)
            }),
        ),
        6 => (
            Kind::Benign,
            long(MavCmd::MAV_CMD_COMPONENT_ARM_DISARM, 0.0, 0.0),
        ),
        7 => (
            Kind::Benign,
            MavMessage::PARAM_REQUEST_READ(PARAM_REQUEST_READ_DATA {
                target_system: fc::SYS,
                target_component: fc::COMP,
                param_index: 3,
                param_id: "SIM_P03".into(),
            }),
        ),
        8 => (
            Kind::Benign,
            long(MavCmd::MAV_CMD_REQUEST_MESSAGE, 245.0, 0.0),
        ),
        _ => (
            Kind::Benign,
            MavMessage::HEARTBEAT(HEARTBEAT_DATA {
                mavtype: MavType::MAV_TYPE_GCS,
                autopilot: MavAutopilot::MAV_AUTOPILOT_INVALID,
                mavlink_version: 3,
                ..Default::default()
            }),
        ),
    };
    (kind, g.frame(&msg))
}

/// Whether a frame the FC decoded is one of the [`Kind::Flight`] commands
/// [`command`] makes (the oracle's own list, not the router's).
pub fn is_flight(h: &crate::fc::Heard) -> bool {
    match (h.msg_id, h.command) {
        (69 | 70 | 73, _) => true,
        (76, Some((400, p1, _))) => p1 > 0.5,
        (76, Some((176 | 22, _, _))) => true,
        _ => false,
    }
}
