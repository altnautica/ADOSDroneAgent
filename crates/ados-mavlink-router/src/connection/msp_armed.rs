//! Passive armed-state read for an MSP flight controller (Betaflight / iNav).
//!
//! The router is a byte-pipe for MSP: a GCS on the transparent proxy polls the
//! FC and the FC's replies flow back through [`super::reader`]. This sniffer
//! watches those replies, observe-only, and latches the armed flag from the
//! status responses a poller asks for (`MSP_STATUS`, `MSP_STATUS_EX` on
//! Betaflight; `MSP2_INAV_STATUS` on iNav). Nothing is sent to the FC: with no
//! poller attached the flag simply goes stale and reads as unknown.

/// `MSP_STATUS` (101): U16 cycleTime, U16 i2cErrors, U16 sensors, U32 flags.
const MSP_STATUS: u16 = 101;
/// `MSP_STATUS_EX` (150): the `MSP_STATUS` prefix plus more.
const MSP_STATUS_EX: u16 = 150;
/// `MSP2_INAV_STATUS` (0x2000): U16 cycleTime, U16 i2cErrors, U16 sensors,
/// U16 load, U8 profiles, U32 armingFlags, ...
const MSP2_INAV_STATUS: u16 = 0x2000;
/// Largest payload buffered; status frames are a few dozen bytes.
const MAX_PAYLOAD: usize = 512;

/// The armed flag in one decoded MSP response, or `None` when the frame is not
/// a status response for this firmware or is too short to carry the flag.
pub(crate) fn armed_from_frame(variant: &str, cmd: u16, payload: &[u8]) -> Option<bool> {
    let inav = variant.eq_ignore_ascii_case("inav");
    let u32_at = |at: usize| -> Option<u32> {
        payload
            .get(at..at + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    match cmd {
        // iNav's armingFlags bit 2 is ARMED.
        MSP2_INAV_STATUS if inav => u32_at(9).map(|f| f & (1 << 2) != 0),
        // Betaflight's box-mode flags bit 0 is the ARM box.
        MSP_STATUS | MSP_STATUS_EX if !inav => u32_at(6).map(|f| f & 1 != 0),
        _ => None,
    }
}

/// CRC-8/DVB-S2 over one byte, the MSP v2 checksum.
fn crc8_dvb_s2(crc: u8, byte: u8) -> u8 {
    let mut crc = crc ^ byte;
    for _ in 0..8 {
        crc = if crc & 0x80 != 0 {
            (crc << 1) ^ 0xD5
        } else {
            crc << 1
        };
    }
    crc
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Dollar,
    /// After `$M` / `$X`; the bool is v2.
    Proto(bool),
    V1Size,
    V1Cmd,
    V1Payload,
    V1Check,
    V2Flag,
    V2Cmd0,
    V2Cmd1,
    V2Size0,
    V2Size1,
    V2Payload,
    V2Check,
}

/// A streaming MSP v1/v2 response parser over the FC->host byte stream.
#[derive(Debug)]
pub(crate) struct MspResponseSniffer {
    state: State,
    size: usize,
    cmd: u16,
    check: u8,
    payload: Vec<u8>,
}

impl Default for MspResponseSniffer {
    fn default() -> Self {
        Self {
            state: State::Idle,
            size: 0,
            cmd: 0,
            check: 0,
            payload: Vec::with_capacity(64),
        }
    }
}

impl MspResponseSniffer {
    /// Feed inbound bytes; `on_frame(cmd, payload)` runs for every complete,
    /// checksum-valid response (`$M>` / `$X>`). Error replies (`!`) and
    /// requests (`<`) are skipped.
    pub(crate) fn feed(&mut self, bytes: &[u8], mut on_frame: impl FnMut(u16, &[u8])) {
        for &b in bytes {
            self.state = match self.state {
                State::Idle => {
                    if b == b'$' {
                        State::Dollar
                    } else {
                        State::Idle
                    }
                }
                State::Dollar => match b {
                    b'M' => State::Proto(false),
                    b'X' => State::Proto(true),
                    b'$' => State::Dollar,
                    _ => State::Idle,
                },
                State::Proto(v2) => match (b, v2) {
                    (b'>', false) => State::V1Size,
                    (b'>', true) => State::V2Flag,
                    // A broken header followed by a new frame start resyncs.
                    (b'$', _) => State::Dollar,
                    _ => State::Idle,
                },
                State::V1Size => {
                    // 255 announces a jumbo frame; status replies never are.
                    if b == 255 {
                        State::Idle
                    } else {
                        self.size = b as usize;
                        self.check = b;
                        self.payload.clear();
                        State::V1Cmd
                    }
                }
                State::V1Cmd => {
                    self.cmd = b as u16;
                    self.check ^= b;
                    if self.size == 0 {
                        State::V1Check
                    } else {
                        State::V1Payload
                    }
                }
                State::V1Payload => {
                    self.payload.push(b);
                    self.check ^= b;
                    if self.payload.len() == self.size {
                        State::V1Check
                    } else {
                        State::V1Payload
                    }
                }
                State::V1Check => {
                    if b == self.check {
                        on_frame(self.cmd, &self.payload);
                    }
                    State::Idle
                }
                State::V2Flag => {
                    self.check = crc8_dvb_s2(0, b);
                    State::V2Cmd0
                }
                State::V2Cmd0 => {
                    self.check = crc8_dvb_s2(self.check, b);
                    self.cmd = b as u16;
                    State::V2Cmd1
                }
                State::V2Cmd1 => {
                    self.check = crc8_dvb_s2(self.check, b);
                    self.cmd |= (b as u16) << 8;
                    State::V2Size0
                }
                State::V2Size0 => {
                    self.check = crc8_dvb_s2(self.check, b);
                    self.size = b as usize;
                    State::V2Size1
                }
                State::V2Size1 => {
                    self.check = crc8_dvb_s2(self.check, b);
                    self.size |= (b as usize) << 8;
                    self.payload.clear();
                    if self.size > MAX_PAYLOAD {
                        State::Idle
                    } else if self.size == 0 {
                        State::V2Check
                    } else {
                        State::V2Payload
                    }
                }
                State::V2Payload => {
                    self.check = crc8_dvb_s2(self.check, b);
                    self.payload.push(b);
                    if self.payload.len() == self.size {
                        State::V2Check
                    } else {
                        State::V2Payload
                    }
                }
                State::V2Check => {
                    if b == self.check {
                        on_frame(self.cmd, &self.payload);
                    }
                    State::Idle
                }
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v1(cmd: u8, payload: &[u8]) -> Vec<u8> {
        let mut f = vec![b'$', b'M', b'>', payload.len() as u8, cmd];
        f.extend_from_slice(payload);
        f.push(payload.iter().fold(payload.len() as u8 ^ cmd, |c, b| c ^ b));
        f
    }

    fn v2(cmd: u16, payload: &[u8]) -> Vec<u8> {
        let len = payload.len() as u16;
        let body: Vec<u8> = [0u8]
            .into_iter()
            .chain(cmd.to_le_bytes())
            .chain(len.to_le_bytes())
            .chain(payload.iter().copied())
            .collect();
        let mut f = vec![b'$', b'X', b'>'];
        f.extend_from_slice(&body);
        f.push(body.iter().fold(0, |c, &b| crc8_dvb_s2(c, b)));
        f
    }

    fn bf_status(armed: bool) -> Vec<u8> {
        let mut p = vec![0u8; 16];
        p[6] = if armed { 1 } else { 0 };
        p
    }

    fn inav_status(armed: bool) -> Vec<u8> {
        let mut p = vec![0u8; 20];
        p[9] = if armed { 1 << 2 } else { 0 };
        p
    }

    fn sniff(bytes: &[u8], variant: &str) -> Vec<bool> {
        let mut s = MspResponseSniffer::default();
        let mut out = Vec::new();
        s.feed(bytes, |cmd, p| {
            out.extend(armed_from_frame(variant, cmd, p))
        });
        out
    }

    #[test]
    fn betaflight_status_ex_reports_the_arm_box() {
        assert_eq!(sniff(&v1(150, &bf_status(true)), "betaflight"), vec![true]);
        assert_eq!(
            sniff(&v1(101, &bf_status(false)), "betaflight"),
            vec![false]
        );
    }

    #[test]
    fn inav_status_reports_the_armed_arming_flag() {
        assert_eq!(sniff(&v2(0x2000, &inav_status(true)), "inav"), vec![true]);
        assert_eq!(sniff(&v2(0x2000, &inav_status(false)), "iNav"), vec![false]);
        // A Betaflight-layout status is not read on iNav.
        assert!(sniff(&v1(150, &bf_status(true)), "inav").is_empty());
    }

    #[test]
    fn frames_split_across_reads_and_noise_still_decode() {
        let mut bytes = b"junk$$M".to_vec();
        bytes.extend(v1(150, &bf_status(true)));
        let mut s = MspResponseSniffer::default();
        let mut out = Vec::new();
        for chunk in bytes.chunks(3) {
            s.feed(chunk, |cmd, p| {
                out.extend(armed_from_frame("betaflight", cmd, p))
            });
        }
        assert_eq!(out, vec![true]);
    }

    #[test]
    fn bad_checksums_requests_and_short_frames_are_ignored() {
        let mut bad = v1(150, &bf_status(true));
        *bad.last_mut().unwrap() ^= 0xFF;
        assert!(sniff(&bad, "betaflight").is_empty());
        let mut req = v1(150, &bf_status(true));
        req[2] = b'<';
        assert!(sniff(&req, "betaflight").is_empty());
        assert!(sniff(&v1(150, &[0u8; 4]), "betaflight").is_empty());
        let mut bad2 = v2(0x2000, &inav_status(true));
        *bad2.last_mut().unwrap() ^= 0x01;
        assert!(sniff(&bad2, "inav").is_empty());
    }
}
