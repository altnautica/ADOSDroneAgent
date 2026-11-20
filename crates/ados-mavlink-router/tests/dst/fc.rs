//! A simulated ArduPilot copter, as far as the router sees one: a 1 Hz
//! HEARTBEAT, the streams a `SET_MESSAGE_INTERVAL` asked for (forgotten at
//! every boot, as ArduPilot forgets them), `COMMAND_ACK`s, the firmware
//! banner, a parameter list, and the mission protocol both ways (the
//! router's or a ground station's download, a ground station's upload with
//! ArduPilot's request retries). Its mission survives a reboot.

use std::collections::BTreeMap;

use ados_protocol::mavlink::ardupilotmega::*;
use ados_protocol::mavlink::{MavHeader, Message};

use crate::mav;
use crate::rng::Rng;

pub const SYS: u8 = 1;
pub const COMP: u8 = 1;
/// The ids this FC can stream.
const STREAMABLE: [u32; 5] = [1, 30, 33, 42, 245];
const PARAMS: usize = 24;
/// An upload's per-request wait and tries before the FC cancels it.
const UPLOAD_RETRY_MS: u64 = 1_000;
const UPLOAD_TRIES: u8 = 5;
/// After an upload, a repeated last item is answered with the ACK again.
const REACK_MS: u64 = 5_000;

/// One frame the FC decoded from its link, for the invariants.
#[derive(Debug, Clone)]
pub struct Heard {
    pub sys: u8,
    pub comp: u8,
    pub msg_id: u32,
    /// The `COMMAND_LONG` command and `param1`/`param2`.
    pub command: Option<(u16, f32, f32)>,
}

struct Upload {
    from: (u8, u8),
    count: u16,
    items: Vec<MISSION_ITEM_INT_DATA>,
    sent_at: u64,
    tries: u8,
}

pub struct Fc {
    pub powered: bool,
    off_until: u64,
    boot_at: u64,
    pub boots: u32,
    seq: u8,
    /// Stream intervals, ms, by message id: the boot defaults (the FC's own
    /// stream-rate parameters) plus what `SET_MESSAGE_INTERVAL` asked for.
    rates: BTreeMap<u32, u64>,
    /// The streams the FC sends from boot, unasked.
    defaults: BTreeMap<u32, u64>,
    next_at: BTreeMap<u32, u64>,
    next_hb: u64,
    pub mission: Vec<MISSION_ITEM_INT_DATA>,
    upload: Option<Upload>,
    done: Option<((u8, u8), u64)>,
    param_next: Option<usize>,
    replies: Vec<Vec<u8>>,
    /// Every frame decoded since the harness last drained it.
    pub heard: Vec<Heard>,
    /// Bytes received from the router, ever.
    pub rx_bytes: u64,
}

pub fn waypoint(seq: u16, lat_e7: i32, lon_e7: i32, alt: f32) -> MISSION_ITEM_INT_DATA {
    MISSION_ITEM_INT_DATA {
        x: lat_e7,
        y: lon_e7,
        z: alt,
        seq,
        command: MavCmd::MAV_CMD_NAV_WAYPOINT,
        frame: MavFrame::MAV_FRAME_GLOBAL_RELATIVE_ALT,
        autocontinue: 1,
        ..Default::default()
    }
}

/// A random mission: the home slot plus `n` waypoints.
pub fn random_mission(rng: &mut Rng, n: u16) -> Vec<MISSION_ITEM_INT_DATA> {
    (0..=n)
        .map(|seq| {
            let lat = 473_977_000 + rng.between(0, 20_000) as i32;
            let lon = 85_456_000 + rng.between(0, 20_000) as i32;
            waypoint(seq, lat, lon, rng.range(5.0, 60.0).round())
        })
        .collect()
}

impl Fc {
    pub fn new(mission: Vec<MISSION_ITEM_INT_DATA>) -> Self {
        Self {
            powered: true,
            off_until: 0,
            boot_at: 0,
            boots: 1,
            seq: 0,
            rates: BTreeMap::new(),
            defaults: BTreeMap::new(),
            next_at: BTreeMap::new(),
            next_hb: 0,
            mission,
            upload: None,
            done: None,
            param_next: None,
            replies: Vec::new(),
            heard: Vec::new(),
            rx_bytes: 0,
        }
    }

    /// Power the FC off until `until`; it boots then with no stream requested.
    pub fn power_off(&mut self, until: u64) {
        self.powered = false;
        self.off_until = until;
    }

    /// Whether `id` is streaming.
    pub fn streaming(&self, id: u32) -> bool {
        self.powered && self.rates.contains_key(&id)
    }

    /// Stream `id` from boot every `interval_ms`, unasked.
    pub fn stream_by_default(&mut self, id: u32, interval_ms: u64) {
        self.defaults.insert(id, interval_ms);
        self.rates.insert(id, interval_ms);
    }

    fn frame(&mut self, msg: &MavMessage) -> Vec<u8> {
        self.seq = self.seq.wrapping_add(1);
        mav::encode(SYS, COMP, self.seq, msg)
    }

    fn reply(&mut self, msg: MavMessage) {
        let f = self.frame(&msg);
        self.replies.push(f);
    }

    /// Bytes from the router.
    pub fn on_bytes(&mut self, now: u64, bytes: &[u8]) {
        self.rx_bytes += bytes.len() as u64;
        if !self.powered {
            return;
        }
        for (h, msg, _) in mav::decode(bytes) {
            self.on_frame(now, h, msg);
        }
    }

    fn on_frame(&mut self, now: u64, h: MavHeader, msg: MavMessage) {
        let from = (h.system_id, h.component_id);
        let command = match &msg {
            MavMessage::COMMAND_LONG(c) => Some((c.command as u16, c.param1, c.param2)),
            _ => None,
        };
        self.heard.push(Heard {
            sys: from.0,
            comp: from.1,
            msg_id: msg.message_id(),
            command,
        });
        match msg {
            MavMessage::COMMAND_LONG(c) => self.on_command(&c),
            MavMessage::PARAM_REQUEST_LIST(_) => self.param_next = Some(0),
            MavMessage::MISSION_REQUEST_LIST(_) => {
                self.reply(MavMessage::MISSION_COUNT(MISSION_COUNT_DATA {
                    count: self.mission.len() as u16,
                    target_system: from.0,
                    target_component: from.1,
                }))
            }
            MavMessage::MISSION_REQUEST_INT(r) => self.send_item(from, r.seq),
            MavMessage::MISSION_COUNT(c) => self.start_upload(now, from, c.count),
            MavMessage::MISSION_ITEM_INT(item) => self.take_item(now, from, item),
            _ => {}
        }
    }

    fn on_command(&mut self, c: &COMMAND_LONG_DATA) {
        match c.command {
            MavCmd::MAV_CMD_SET_MESSAGE_INTERVAL => {
                let id = c.param1 as u32;
                if c.param2 > 0.0 && STREAMABLE.contains(&id) {
                    self.rates.insert(id, (c.param2 / 1000.0).max(1.0) as u64);
                } else if c.param2 <= 0.0 {
                    self.rates.remove(&id);
                }
            }
            MavCmd::MAV_CMD_DO_SEND_BANNER => {
                self.reply(MavMessage::STATUSTEXT(STATUSTEXT_DATA {
                    severity: MavSeverity::MAV_SEVERITY_INFO,
                    text: "ArduCopter V4.5.7 (2a3dc4b2)".into(),
                }));
            }
            _ => {}
        }
        self.reply(MavMessage::COMMAND_ACK(COMMAND_ACK_DATA {
            command: c.command,
            result: MavResult::MAV_RESULT_ACCEPTED,
        }));
    }

    fn send_item(&mut self, to: (u8, u8), seq: u16) {
        if self.upload.as_ref().is_some_and(|u| u.from == to) {
            return;
        }
        let msg = match self.mission.get(usize::from(seq)) {
            Some(item) => MavMessage::MISSION_ITEM_INT(MISSION_ITEM_INT_DATA {
                target_system: to.0,
                target_component: to.1,
                ..item.clone()
            }),
            None => ack(to, MavMissionResult::MAV_MISSION_INVALID_SEQUENCE),
        };
        self.reply(msg);
    }

    fn request(&mut self, to: (u8, u8), seq: u16) {
        self.reply(MavMessage::MISSION_REQUEST_INT(MISSION_REQUEST_INT_DATA {
            seq,
            target_system: to.0,
            target_component: to.1,
        }));
    }

    fn start_upload(&mut self, now: u64, from: (u8, u8), count: u16) {
        if self.upload.as_ref().is_some_and(|u| u.from != from) {
            self.reply(ack(from, MavMissionResult::MAV_MISSION_DENIED));
            return;
        }
        if count == 0 {
            self.mission.clear();
            self.upload = None;
            self.done = Some((from, now));
            self.reply(ack(from, MavMissionResult::MAV_MISSION_ACCEPTED));
            return;
        }
        self.upload = Some(Upload {
            from,
            count,
            items: Vec::new(),
            sent_at: now,
            tries: 0,
        });
        self.request(from, 0);
    }

    fn take_item(&mut self, now: u64, from: (u8, u8), item: MISSION_ITEM_INT_DATA) {
        let Some(up) = self.upload.as_mut().filter(|u| u.from == from) else {
            if self
                .done
                .is_some_and(|(f, at)| f == from && now - at <= REACK_MS)
            {
                self.reply(ack(from, MavMissionResult::MAV_MISSION_ACCEPTED));
            }
            return;
        };
        let next = up.items.len() as u16;
        if item.seq == next {
            up.items.push(item);
            up.sent_at = now;
            up.tries = 0;
        }
        let next = up.items.len() as u16;
        if next == up.count {
            let up = self.upload.take().expect("present");
            self.mission = up.items;
            self.done = Some((from, now));
            self.reply(ack(from, MavMissionResult::MAV_MISSION_ACCEPTED));
        } else {
            self.request(from, next);
        }
    }

    /// The frames the FC sends this tick.
    pub fn tick(&mut self, now: u64) -> Vec<Vec<u8>> {
        if !self.powered {
            if now < self.off_until {
                return Vec::new();
            }
            self.powered = true;
            self.boots += 1;
            self.boot_at = now;
            self.rates = self.defaults.clone();
            self.next_at.clear();
            self.upload = None;
            self.param_next = None;
            self.replies.clear();
            self.next_hb = now + 300;
        }
        let mut out = std::mem::take(&mut self.replies);
        if now >= self.next_hb {
            self.next_hb = now + 1_000;
            let hb = MavMessage::HEARTBEAT(HEARTBEAT_DATA {
                custom_mode: 5,
                mavtype: MavType::MAV_TYPE_QUADROTOR,
                autopilot: MavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA,
                base_mode: MavModeFlag::MAV_MODE_FLAG_CUSTOM_MODE_ENABLED,
                system_status: MavState::MAV_STATE_STANDBY,
                mavlink_version: 3,
            });
            out.push(self.frame(&hb));
        }
        let due: Vec<(u32, u64)> = self
            .rates
            .iter()
            .filter(|(id, _)| self.next_at.get(id).is_none_or(|t| *t <= now))
            .map(|(id, iv)| (*id, *iv))
            .collect();
        for (id, iv) in due {
            self.next_at.insert(id, now + iv);
            out.push(self.stream(now, id));
        }
        if let Some(i) = self.param_next {
            for i in i..(i + 2).min(PARAMS) {
                let msg = MavMessage::PARAM_VALUE(PARAM_VALUE_DATA {
                    param_value: i as f32,
                    param_count: PARAMS as u16,
                    param_index: i as u16,
                    param_id: format!("SIM_P{i:02}").as_str().into(),
                    param_type: MavParamType::MAV_PARAM_TYPE_REAL32,
                });
                out.push(self.frame(&msg));
            }
            self.param_next = Some(i + 2).filter(|n| *n < PARAMS);
        }
        if let Some(up) = self.upload.as_mut() {
            if now - up.sent_at >= UPLOAD_RETRY_MS {
                up.sent_at = now;
                up.tries += 1;
                let (from, next, give_up) =
                    (up.from, up.items.len() as u16, up.tries > UPLOAD_TRIES);
                if give_up {
                    self.upload = None;
                    out.push(self.frame(&ack(
                        from,
                        MavMissionResult::MAV_MISSION_OPERATION_CANCELLED,
                    )));
                } else {
                    let r = MavMessage::MISSION_REQUEST_INT(MISSION_REQUEST_INT_DATA {
                        seq: next,
                        target_system: from.0,
                        target_component: from.1,
                    });
                    out.push(self.frame(&r));
                }
            }
        }
        out
    }

    fn stream(&mut self, now: u64, id: u32) -> Vec<u8> {
        let t = (now - self.boot_at) as f32 / 1000.0;
        let msg = match id {
            30 => MavMessage::ATTITUDE(ATTITUDE_DATA {
                time_boot_ms: (now - self.boot_at) as u32,
                roll: 0.05 * t.sin(),
                pitch: 0.04 * t.cos(),
                yaw: 0.3,
                ..Default::default()
            }),
            1 => MavMessage::SYS_STATUS(SYS_STATUS_DATA {
                voltage_battery: 15_800,
                battery_remaining: 87,
                ..Default::default()
            }),
            33 => MavMessage::GLOBAL_POSITION_INT(GLOBAL_POSITION_INT_DATA {
                time_boot_ms: (now - self.boot_at) as u32,
                lat: 473_977_000,
                lon: 85_456_000,
                alt: 488_000,
                ..Default::default()
            }),
            245 => MavMessage::EXTENDED_SYS_STATE(EXTENDED_SYS_STATE_DATA {
                landed_state: MavLandedState::MAV_LANDED_STATE_ON_GROUND,
                ..Default::default()
            }),
            _ => {
                let n = self.mission.len() as u16;
                self.seq = self.seq.wrapping_add(1);
                let (cur, state) = if n <= 1 { (0, 1) } else { (1, 2) };
                return mav::mission_current(SYS, COMP, self.seq, cur, n.saturating_sub(1), state);
            }
        };
        self.frame(&msg)
    }
}

fn ack(to: (u8, u8), result: MavMissionResult) -> MavMessage {
    MavMessage::MISSION_ACK(MISSION_ACK_DATA {
        target_system: to.0,
        target_component: to.1,
        mavtype: result,
    })
}
