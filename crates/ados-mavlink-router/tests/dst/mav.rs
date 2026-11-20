//! MAVLink framing for the simulated peers: encode a message, split a byte
//! run into whole frames, and hand-encode the `MISSION_CURRENT` extension
//! fields (`total`, `mission_state`) this build's dialect does not carry.

use ados_protocol::mavlink::ardupilotmega::MavMessage;
use ados_protocol::mavlink::{self, MavHeader};

/// Serialise `msg` as MAVLink 2 from `(sys, comp)`.
pub fn encode(sys: u8, comp: u8, seq: u8, msg: &MavMessage) -> Vec<u8> {
    let header = MavHeader {
        system_id: sys,
        component_id: comp,
        sequence: seq,
    };
    mavlink::serialize_v2(header, msg).expect("a dialect message serialises")
}

/// The MAVLink X.25 checksum step.
fn x25(b: u8, crc: u16) -> u16 {
    let mut t = b ^ (crc & 0xFF) as u8;
    t ^= t << 4;
    let t = u16::from(t);
    (crc >> 8) ^ (t << 8) ^ (t << 3) ^ (t >> 4)
}

/// A MAVLink 2 frame for `msg_id` with `payload` (trailing zeros trimmed, as
/// the wire does) and a valid checksum over `crc_extra`.
fn raw_v2(sys: u8, comp: u8, seq: u8, msg_id: u32, crc_extra: u8, payload: &[u8]) -> Vec<u8> {
    let mut p = payload.to_vec();
    while p.len() > 1 && p.last() == Some(&0) {
        p.pop();
    }
    let mut f = vec![0xFD, p.len() as u8, 0, 0, seq, sys, comp];
    f.extend_from_slice(&msg_id.to_le_bytes()[..3]);
    f.extend_from_slice(&p);
    let crc = f[1..].iter().fold(0xFFFF, |c, b| x25(*b, c));
    let crc = x25(crc_extra, crc);
    f.extend_from_slice(&crc.to_le_bytes());
    f
}

/// `MISSION_CURRENT` (42, CRC extra 28) with its `total` and `mission_state`.
pub fn mission_current(sys: u8, comp: u8, seq: u8, current: u16, total: u16, state: u8) -> Vec<u8> {
    let mut p = Vec::with_capacity(6);
    p.extend_from_slice(&current.to_le_bytes());
    p.extend_from_slice(&total.to_le_bytes());
    p.push(state);
    p.push(0);
    raw_v2(sys, comp, seq, 42, 28, &p)
}

/// Every whole MAVLink 1 or 2 frame in `buf`, in order; junk and a truncated
/// tail are skipped.
pub fn split(buf: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < buf.len() {
        let total = match buf[i] {
            0xFD if i + 3 <= buf.len() => {
                let signed = buf[i + 2] & 0x01 != 0;
                12 + usize::from(buf[i + 1]) + if signed { 13 } else { 0 }
            }
            0xFE if i + 2 <= buf.len() => 8 + usize::from(buf[i + 1]),
            _ => {
                i += 1;
                continue;
            }
        };
        if i + total > buf.len() {
            break;
        }
        out.push(&buf[i..i + total]);
        i += total;
    }
    out
}

/// Decode every frame of `buf` that parses, with its sender.
pub fn decode(buf: &[u8]) -> Vec<(MavHeader, MavMessage, Vec<u8>)> {
    split(buf)
        .into_iter()
        .filter_map(|f| mavlink::parse_any(f).ok().map(|(h, m)| (h, m, f.to_vec())))
        .collect()
}
