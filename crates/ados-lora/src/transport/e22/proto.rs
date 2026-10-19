//! The E22 UART command set and the receive-buffer scanner (Ebyte E22-900T22D
//! user manual v1.3, sections 6 and 7).
//!
//! Register map (section 7.2): `00H ADDH`, `01H ADDL`, `02H NETID`,
//! `03H REG0` (UART rate bits 7-5, parity bits 4-3, air data rate bits 2-0),
//! `04H REG1` (sub-packet bits 7-6, ambient-noise RSSI bit 5, TX power bits
//! 1-0), `05H REG2` (channel), `06H REG3` (RSSI byte bit 7, fixed-point bit 6,
//! repeater bit 5, LBT bit 4, WOR role bit 3, WOR cycle bits 2-0), and the
//! read-only product information `80H..86H`.
//!
//! Commands in configuration mode (section 7.1): `C0 addr len data` writes and
//! saves, `C2 addr len data` writes without saving, `C1 addr len` reads; every
//! one is answered `C1 addr len data`, a malformed one with `FF FF FF`.

use std::collections::VecDeque;

use crate::band::{ModuleCaps, Tuning};
use crate::frame::{Frame, FrameError};
use crate::transport::RxEvent;

pub const CMD_WRITE_SAVE: u8 = 0xC0;
pub const CMD_READ: u8 = 0xC1;
pub const CMD_WRITE_TEMP: u8 = 0xC2;
/// Every register reply starts with this byte.
pub const REPLY: u8 = 0xC1;
const WRONG_FORMAT: [u8; 3] = [0xFF, 0xFF, 0xFF];

pub const REG_ADDH: u8 = 0x00;
pub const REG_REG1: u8 = 0x04;
pub const REG_REG2: u8 = 0x05;
pub const REG_REG3: u8 = 0x06;
pub const REG_PID: u8 = 0x80;
pub const PID_LEN: u8 = 7;

/// Module address `FFFF`: broadcast and monitor address, no address filtering
/// on receive (section 7.2, ADDH/ADDL remark). Also the fixed-point target.
pub const BROADCAST_ADDR: [u8; 2] = [0xFF, 0xFF];
pub const NETID: u8 = 0x00;

/// REG0: UART 9600 (bits 7-5 `011`), 8N1 (bits 4-3 `00`), air data rate 2.4k
/// (bits 2-0 `010`).
pub const REG0_9600_8N1_2K4: u8 = (0b011 << 5) | 0b010;
/// REG1 bit 5: ambient-noise RSSI readable with `C0 C1 C2 C3`.
pub const REG1_AMBIENT_RSSI: u8 = 1 << 5;
/// REG3 bit 7: the module appends an RSSI byte to every received packet.
pub const REG3_RSSI_BYTE: u8 = 1 << 7;
/// REG3 bit 6: fixed-point transmission (first three bytes are the target).
pub const REG3_FIXED_POINT: u8 = 1 << 6;
/// REG3 bit 4: the module's own listen-before-talk.
pub const REG3_LBT: u8 = 1 << 4;
/// REG3 bits 2-0: WOR cycle 2000 ms, the factory value (used in mode 1 only).
pub const REG3_WOR_2000MS: u8 = 0b011;

/// Ambient-noise RSSI query in normal mode: `C0 C1 C2 C3 + 00 + 01` reads
/// register 0 (current noise), answered `C1 00 01 rssi` (REG1 remark).
pub const AMBIENT_QUERY: [u8; 6] = [0xC0, 0xC1, 0xC2, 0xC3, 0x00, 0x01];
pub const AMBIENT_REPLY: [u8; 3] = [REPLY, 0x00, 0x01];

/// The REG1 power bits for a conducted output in dBm (`00` 22, `01` 17,
/// `10` 13, `11` 10).
pub fn power_bits(dbm: i8) -> Option<u8> {
    match dbm {
        22 => Some(0b00),
        17 => Some(0b01),
        13 => Some(0b10),
        10 => Some(0b11),
        _ => None,
    }
}

/// REG1: 240-byte sub-packets, ambient-noise RSSI on, `dbm` output.
pub fn reg1(dbm: i8) -> Option<u8> {
    power_bits(dbm).map(|p| REG1_AMBIENT_RSSI | p)
}

/// REG3: RSSI byte on, fixed-point on, repeater off, LBT as given, WOR receiver
/// with the factory cycle.
pub fn reg3(lbt: bool) -> u8 {
    let lbt = if lbt { REG3_LBT } else { 0 };
    REG3_RSSI_BYTE | REG3_FIXED_POINT | lbt | REG3_WOR_2000MS
}

/// A register write: `cmd addr len data`.
pub fn write_cmd(cmd: u8, addr: u8, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(3 + data.len());
    out.extend_from_slice(&[cmd, addr, data.len() as u8]);
    out.extend_from_slice(data);
    out
}

/// The reply header a command at `addr` of `len` bytes is answered with.
pub fn reply_header(addr: u8, len: u8) -> [u8; 3] {
    [REPLY, addr, len]
}

/// The channel register value for a carrier on `mhz`
/// (`f = base + CH x 1 MHz`), or `None` when the module cannot tune it.
pub fn channel_index(caps: &ModuleCaps, mhz: f64) -> Option<u8> {
    let Tuning::Grid {
        start_mhz,
        step_mhz,
        ..
    } = caps.tuning
    else {
        return None;
    };
    if !caps.can_tune(mhz) {
        return None;
    }
    u8::try_from(((mhz - start_mhz) / step_mhz).round() as i64).ok()
}

/// A module RSSI byte in dBm: `-(256 - v)`.
pub fn rssi_dbm(v: u8) -> i16 {
    -(256 - i16::from(v))
}

/// What one scan of the receive buffer produced.
#[derive(Debug, PartialEq)]
pub enum Scan {
    /// The buffer holds no complete unit; read more.
    NeedMore,
    /// A frame was queued or a garbage byte dropped; scan again.
    Progress,
    /// The awaited register reply, its data bytes.
    Reply(Vec<u8>),
    /// The module answered `FF FF FF` (malformed command).
    Wrong,
}

/// Bytes read from the UART that are not yet consumed, and the frames already
/// split out of them.
#[derive(Debug, Default)]
pub struct RxBuf {
    pub buf: Vec<u8>,
    pub pending: VecDeque<RxEvent>,
}

impl RxBuf {
    /// Consume the next unit at the head of the buffer. With `reply` set, a
    /// register reply with that header is looked for as well; frames received
    /// before it are queued, and unparseable bytes in front of it dropped.
    pub fn scan(&mut self, reply: Option<[u8; 3]>) -> Scan {
        if self.buf.is_empty() {
            return Scan::NeedMore;
        }
        match self.head_frame() {
            Head::Frame => return Scan::Progress,
            Head::Partial if reply.is_none() => return Scan::NeedMore,
            Head::Garbage if reply.is_none() => {
                self.buf.remove(0);
                return Scan::Progress;
            }
            _ => {}
        }
        let Some(header) = reply else {
            return Scan::NeedMore;
        };
        if let Some(at) = find(&self.buf, &header) {
            let end = at + header.len() + usize::from(header[2]);
            if self.buf.len() < end {
                return Scan::NeedMore;
            }
            let data = self.buf[at + header.len()..end].to_vec();
            let before: Vec<u8> = self.buf.drain(..end).take(at).collect();
            self.queue_frames(&before);
            return Scan::Reply(data);
        }
        if let Some(at) = find(&self.buf, &WRONG_FORMAT) {
            self.buf.drain(..at + WRONG_FORMAT.len());
            return Scan::Wrong;
        }
        Scan::NeedMore
    }

    /// Try to take one frame plus its trailing RSSI byte off the head.
    fn head_frame(&mut self) -> Head {
        match Frame::decode(&self.buf) {
            Ok((_, used)) if self.buf.len() > used => {
                let rssi = self.buf[used];
                let bytes: Vec<u8> = self.buf.drain(..=used).take(used).collect();
                self.pending.push_back(RxEvent::Frame {
                    bytes,
                    rssi_dbm: Some(rssi_dbm(rssi)),
                    snr_db: None,
                });
                Head::Frame
            }
            Ok(_) | Err(FrameError::Short) => Head::Partial,
            Err(_) => Head::Garbage,
        }
    }

    /// Queue every whole frame (with its RSSI byte) found in `bytes`.
    fn queue_frames(&mut self, bytes: &[u8]) {
        let mut at = 0;
        while at < bytes.len() {
            match Frame::decode(&bytes[at..]) {
                Ok((_, used)) if at + used < bytes.len() => {
                    self.pending.push_back(RxEvent::Frame {
                        bytes: bytes[at..at + used].to_vec(),
                        rssi_dbm: Some(rssi_dbm(bytes[at + used])),
                        snr_db: None,
                    });
                    at += used + 1;
                }
                _ => at += 1,
            }
        }
    }
}

enum Head {
    Frame,
    Partial,
    Garbage,
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}
