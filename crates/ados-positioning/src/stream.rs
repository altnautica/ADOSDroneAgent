//! Splits one receiver byte stream into UBX, NMEA and RTCM3 frames.
//!
//! Receivers interleave protocols on one port, and a read can end anywhere,
//! so the splitter buffers across pushes. Each candidate frame is accepted
//! only when its checksum holds; on a mismatch, or on bytes that start no
//! known frame, one byte is dropped and the scan resumes (resync).

/// Largest UBX payload accepted; anything longer is treated as garbage.
const UBX_MAX_PAYLOAD: usize = 8192;
/// NMEA 0183 caps a sentence at 82 characters; allow some slack.
const NMEA_MAX: usize = 120;
/// Buffer cap: a stream that never forms a frame cannot grow memory.
const BUF_MAX: usize = 64 * 1024;

/// One whole protocol frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// UBX message: class, id, payload.
    Ubx {
        /// Message class.
        class: u8,
        /// Message id.
        id: u8,
        /// Payload without header or checksum.
        payload: Vec<u8>,
    },
    /// NMEA sentence without the trailing CR LF (`$GNGGA,...*hh`).
    Nmea(String),
    /// RTCM3 frame, header to CRC inclusive.
    Rtcm(Vec<u8>),
}

/// The UBX 8-bit Fletcher checksum over class, id, length and payload.
pub fn ubx_checksum(bytes: &[u8]) -> [u8; 2] {
    let (mut a, mut b) = (0u8, 0u8);
    for &x in bytes {
        a = a.wrapping_add(x);
        b = b.wrapping_add(a);
    }
    [a, b]
}

/// CRC-24Q (the RTCM3 / Qualcomm polynomial 0x1864CFB).
pub fn crc24q(bytes: &[u8]) -> u32 {
    let mut crc: u32 = 0;
    for &b in bytes {
        crc ^= (b as u32) << 16;
        for _ in 0..8 {
            crc <<= 1;
            if crc & 0x0100_0000 != 0 {
                crc ^= 0x0186_4CFB;
            }
        }
    }
    crc & 0x00FF_FFFF
}

enum Step {
    Emit(Frame, usize),
    Skip,
    Wait,
}

/// The stream splitter.
#[derive(Debug, Default)]
pub struct Splitter {
    buf: Vec<u8>,
    garbage: u64,
}

impl Splitter {
    /// An empty splitter.
    pub fn new() -> Self {
        Self::default()
    }

    /// Bytes discarded while resynchronising.
    pub fn garbage(&self) -> u64 {
        self.garbage
    }

    /// Append `bytes` and return every frame completed so far.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Frame> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut pos = 0;
        while pos < self.buf.len() {
            match scan(&self.buf[pos..]) {
                Step::Emit(frame, used) => {
                    out.push(frame);
                    pos += used;
                }
                Step::Skip => {
                    // Jump to the next byte that can start a frame.
                    let next = self.buf[pos + 1..]
                        .iter()
                        .position(|&b| matches!(b, 0xB5 | b'$' | 0xD3))
                        .map_or(self.buf.len(), |i| pos + 1 + i);
                    self.garbage += (next - pos) as u64;
                    pos = next;
                }
                Step::Wait => break,
            }
        }
        self.buf.drain(..pos);
        if self.buf.len() > BUF_MAX {
            self.garbage += self.buf.len() as u64;
            self.buf.clear();
        }
        out
    }
}

fn scan(b: &[u8]) -> Step {
    match b[0] {
        0xB5 => scan_ubx(b),
        b'$' => scan_nmea(b),
        0xD3 => scan_rtcm(b),
        _ => Step::Skip,
    }
}

fn scan_ubx(b: &[u8]) -> Step {
    if b.len() < 2 {
        return Step::Wait;
    }
    if b[1] != 0x62 {
        return Step::Skip;
    }
    if b.len() < 6 {
        return Step::Wait;
    }
    let len = u16::from_le_bytes([b[4], b[5]]) as usize;
    if len > UBX_MAX_PAYLOAD {
        return Step::Skip;
    }
    let total = 8 + len;
    if b.len() < total {
        return Step::Wait;
    }
    if ubx_checksum(&b[2..6 + len]) != [b[6 + len], b[7 + len]] {
        return Step::Skip;
    }
    Step::Emit(
        Frame::Ubx {
            class: b[2],
            id: b[3],
            payload: b[6..6 + len].to_vec(),
        },
        total,
    )
}

fn scan_nmea(b: &[u8]) -> Step {
    let window = &b[..b.len().min(NMEA_MAX)];
    let Some(nl) = window.iter().position(|&c| c == b'\n') else {
        return if b.len() < NMEA_MAX {
            Step::Wait
        } else {
            Step::Skip
        };
    };
    let line = &b[..nl];
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    match nmea_valid(line) {
        Some(s) => Step::Emit(Frame::Nmea(s), nl + 1),
        None => Step::Skip,
    }
}

/// Validate `$body*hh` and return it as text.
fn nmea_valid(line: &[u8]) -> Option<String> {
    if line.len() < 4 || line[0] != b'$' {
        return None;
    }
    let star = line.len().checked_sub(3)?;
    if line[star] != b'*' {
        return None;
    }
    let want = u8::from_str_radix(std::str::from_utf8(&line[star + 1..]).ok()?, 16).ok()?;
    let body = &line[1..star];
    if !body.iter().all(|c| (0x20..0x7F).contains(c)) {
        return None;
    }
    let got = body.iter().fold(0u8, |acc, &c| acc ^ c);
    (got == want).then(|| String::from_utf8_lossy(line).into_owned())
}

fn scan_rtcm(b: &[u8]) -> Step {
    if b.len() < 3 {
        return Step::Wait;
    }
    if b[1] & 0xFC != 0 {
        return Step::Skip;
    }
    let len = (((b[1] & 0x03) as usize) << 8) | b[2] as usize;
    let total = 3 + len + 3;
    if b.len() < total {
        return Step::Wait;
    }
    let crc = crc24q(&b[..3 + len]);
    let got = ((b[3 + len] as u32) << 16) | ((b[4 + len] as u32) << 8) | b[5 + len] as u32;
    if crc != got {
        return Step::Skip;
    }
    Step::Emit(Frame::Rtcm(b[..total].to_vec()), total)
}

/// Build a UBX frame with a correct checksum.
pub fn ubx_frame(class: u8, id: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend_from_slice(&[0xB5, 0x62, class, id]);
    out.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    out.extend_from_slice(payload);
    let ck = ubx_checksum(&out[2..]);
    out.extend_from_slice(&ck);
    out
}

/// The RTCM3 message number of a whole frame (first 12 payload bits).
pub fn rtcm_msg_type(frame: &[u8]) -> Option<u16> {
    (frame.len() >= 5).then(|| ((frame[3] as u16) << 4) | (frame[4] as u16 >> 4))
}

#[cfg(test)]
mod tests;
