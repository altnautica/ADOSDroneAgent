//! Raw MAVLink frame access for the mission messages whose MAVLink 2
//! extension fields this build's dialect does not decode (`MISSION_CURRENT`
//! `total`/`mission_state`/`mission_mode`/`mission_id`, the `mission_type` of
//! the transfer messages, `MISSION_ACK.opaque_id`). The frames on the router's
//! fan-out are already CRC-checked by the reader; client frames are checked by
//! the caller's framer. MAVLink 2 truncates trailing zero bytes, so a payload
//! is read zero-padded.

/// One frame's header fields and its payload.
#[derive(Debug, Clone, Copy)]
pub struct RawMsg<'a> {
    pub sys: u8,
    pub comp: u8,
    pub msg_id: u32,
    payload: &'a [u8],
}

impl<'a> RawMsg<'a> {
    fn byte(&self, at: usize) -> u8 {
        self.payload.get(at).copied().unwrap_or(0)
    }

    pub fn u8_at(&self, at: usize) -> u8 {
        self.byte(at)
    }

    pub fn u16_at(&self, at: usize) -> u16 {
        u16::from_le_bytes([self.byte(at), self.byte(at + 1)])
    }

    pub fn u32_at(&self, at: usize) -> u32 {
        u32::from_le_bytes([
            self.byte(at),
            self.byte(at + 1),
            self.byte(at + 2),
            self.byte(at + 3),
        ])
    }
}

/// Every complete MAVLink 1 or 2 frame in `buf`, in order. Bytes that are not
/// a frame start are skipped; a truncated tail is ignored.
pub fn frames(buf: &[u8]) -> Vec<RawMsg<'_>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < buf.len() {
        match buf[i] {
            0xFD if i + 10 <= buf.len() => {
                let len = buf[i + 1] as usize;
                let signed = buf[i + 2] & 0x01 != 0;
                let total = 10 + len + 2 + if signed { 13 } else { 0 };
                if i + total > buf.len() {
                    break;
                }
                out.push(RawMsg {
                    sys: buf[i + 5],
                    comp: buf[i + 6],
                    msg_id: u32::from_le_bytes([buf[i + 7], buf[i + 8], buf[i + 9], 0]),
                    payload: &buf[i + 10..i + 10 + len],
                });
                i += total;
            }
            0xFE if i + 6 <= buf.len() => {
                let len = buf[i + 1] as usize;
                let total = 6 + len + 2;
                if i + total > buf.len() {
                    break;
                }
                out.push(RawMsg {
                    sys: buf[i + 3],
                    comp: buf[i + 4],
                    msg_id: buf[i + 5] as u32,
                    payload: &buf[i + 6..i + 6 + len],
                });
                i += total;
            }
            _ => i += 1,
        }
    }
    out
}

pub const MSG_HEARTBEAT: u32 = 0;
pub const MSG_STATUSTEXT: u32 = 253;
pub const MSG_MISSION_REQUEST: u32 = 40;
pub const MSG_MISSION_CURRENT: u32 = 42;
pub const MSG_MISSION_REQUEST_LIST: u32 = 43;
pub const MSG_MISSION_COUNT: u32 = 44;
pub const MSG_MISSION_ITEM_REACHED: u32 = 46;
pub const MSG_MISSION_ACK: u32 = 47;
pub const MSG_MISSION_REQUEST_INT: u32 = 51;
pub const MSG_MISSION_ITEM_INT: u32 = 73;
pub const MSG_MISSION_ITEM: u32 = 39;
pub const MSG_COMMAND_LONG: u32 = 76;
pub const MSG_COMMAND_ACK: u32 = 77;

/// `MISSION_CURRENT` with its extension fields (`seq`, `total`,
/// `mission_state`, `mission_mode`, `mission_id`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MissionCurrent {
    pub seq: u16,
    pub total: u16,
    pub state: u8,
    pub mode: u8,
    pub id: u32,
}

impl MissionCurrent {
    pub fn decode(m: &RawMsg<'_>) -> Self {
        Self {
            seq: m.u16_at(0),
            total: m.u16_at(2),
            state: m.u8_at(4),
            mode: m.u8_at(5),
            id: m.u32_at(6),
        }
    }
}

/// The `mission_type` extension of a transfer message, 0 (main mission) when
/// absent.
pub fn mission_type(m: &RawMsg<'_>) -> u8 {
    match m.msg_id {
        MSG_MISSION_COUNT => m.u8_at(4),
        MSG_MISSION_ACK => m.u8_at(3),
        MSG_MISSION_REQUEST_LIST => m.u8_at(2),
        MSG_MISSION_REQUEST_INT | MSG_MISSION_REQUEST => m.u8_at(4),
        MSG_MISSION_ITEM_INT | MSG_MISSION_ITEM => m.u8_at(37),
        _ => 0,
    }
}

/// The `(target_system, target_component)` of a transfer message.
pub fn target(m: &RawMsg<'_>) -> (u8, u8) {
    match m.msg_id {
        MSG_MISSION_COUNT => (m.u8_at(2), m.u8_at(3)),
        MSG_MISSION_ACK | MSG_MISSION_REQUEST_LIST => (m.u8_at(0), m.u8_at(1)),
        MSG_MISSION_REQUEST_INT | MSG_MISSION_REQUEST => (m.u8_at(2), m.u8_at(3)),
        MSG_MISSION_ITEM_INT | MSG_MISSION_ITEM => (m.u8_at(32), m.u8_at(33)),
        _ => (0, 0),
    }
}

/// One `MISSION_ITEM_INT`, every field, in the message's field order.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ItemInt {
    pub param1: f32,
    pub param2: f32,
    pub param3: f32,
    pub param4: f32,
    pub x: i32,
    pub y: i32,
    pub z: f32,
    pub seq: u16,
    pub command: u16,
    pub frame: u8,
    pub current: u8,
    pub autocontinue: u8,
}

impl ItemInt {
    pub fn decode(m: &RawMsg<'_>) -> Self {
        let f = |at| f32::from_bits(m.u32_at(at));
        Self {
            param1: f(0),
            param2: f(4),
            param3: f(8),
            param4: f(12),
            x: m.u32_at(16) as i32,
            y: m.u32_at(20) as i32,
            z: f(24),
            seq: m.u16_at(28),
            command: m.u16_at(30),
            frame: m.u8_at(34),
            current: m.u8_at(35),
            autocontinue: m.u8_at(36),
        }
    }
}

#[cfg(test)]
pub(crate) mod build {
    //! Frame builders for the tests: MAVLink 2, unsigned, CRC left zero (the
    //! raw reader does not check it).

    pub fn v2(sys: u8, comp: u8, msg_id: u32, payload: &[u8]) -> Vec<u8> {
        let mut p = payload.to_vec();
        while p.last() == Some(&0) {
            p.pop();
        }
        let mut f = vec![0xFD, p.len() as u8, 0, 0, 0, sys, comp];
        f.extend_from_slice(&msg_id.to_le_bytes()[..3]);
        f.extend_from_slice(&p);
        f.extend_from_slice(&[0, 0]);
        f
    }
}
