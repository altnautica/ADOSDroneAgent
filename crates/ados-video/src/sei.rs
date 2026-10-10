//! In-process H.264 SEI latency-marker injector.
//!
//! When `video.wfb.sei_latency` is on, the primary encoder is split into an
//! encode stage (raw Annex-B on stdout) and a publish stage (Annex-B on stdin).
//! [`pump`] copies the bytes between them and [`SeiInjector`] puts one
//! `user_data_unregistered` SEI NAL carrying the wall-clock time in front of
//! every VCL slice, so every consumer of the published stream sees the same
//! timestamp on the same frame. The payload is the one the ground-side parser
//! (`ados.services.video.sei_parser`) reads: the ADOS latency UUID followed by
//! the nanosecond timestamp, big-endian.

use std::time::{SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The ADOS latency SEI UUID (`ad05140e-9c2c-4f6e-8a31-f0e5b7d4c8a2`).
pub const ADOS_LATENCY_SEI_UUID: [u8; 16] = [
    0xad, 0x05, 0x14, 0x0e, 0x9c, 0x2c, 0x4f, 0x6e, 0x8a, 0x31, 0xf0, 0xe5, 0xb7, 0xd4, 0xc8, 0xa2,
];

/// Annex-B 4-byte start code, used in front of every injected SEI.
const START_CODE: [u8; 4] = [0, 0, 0, 1];
/// NAL header byte for SEI: forbidden_zero_bit 0, nal_ref_idc 0, type 6.
const NAL_HEADER_SEI: u8 = 0x06;
/// SEI payload type `user_data_unregistered`.
const SEI_PAYLOAD_USER_DATA_UNREG: u8 = 0x05;
/// Read size of the pump. At 4 Mbps a 4 KiB read is about 8 ms of video, which
/// bounds the latency the injector adds.
const PUMP_CHUNK: usize = 4096;
/// Most trailing zero bytes held back: the longest prefix of a 4-byte start
/// code that can end a chunk.
const MAX_HELD_ZEROS: usize = 3;

/// Insert H.264 emulation-prevention bytes (§7.4.1.1): after two emitted
/// zeros, any byte `<= 0x03` gets a `0x03` in front of it.
fn emulation_prevent(rbsp: &[u8], out: &mut Vec<u8>) {
    let mut zeros = 0usize;
    for &b in rbsp {
        if zeros >= 2 && b <= 0x03 {
            out.push(0x03);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
}

/// The Annex-B framed SEI NAL carrying `timestamp_ns`:
/// `00 00 00 01 | 06 | 05 18 | UUID[16] | ts_ns[8, BE] | 80`, with
/// emulation prevention applied to everything after the NAL header.
pub fn build_sei_nal(timestamp_ns: u64) -> Vec<u8> {
    let mut rbsp = Vec::with_capacity(27);
    rbsp.push(SEI_PAYLOAD_USER_DATA_UNREG);
    rbsp.push(24);
    rbsp.extend_from_slice(&ADOS_LATENCY_SEI_UUID);
    rbsp.extend_from_slice(&timestamp_ns.to_be_bytes());
    rbsp.push(0x80);
    let mut out = Vec::with_capacity(40);
    out.extend_from_slice(&START_CODE);
    out.push(NAL_HEADER_SEI);
    emulation_prevent(&rbsp, &mut out);
    out
}

/// NAL types 1 (non-IDR slice) and 5 (IDR slice) are VCL slices.
fn is_vcl(nal_byte: u8) -> bool {
    matches!(nal_byte & 0x1f, 1 | 5)
}

/// The next Annex-B start code at or after `from`: `(offset, length)` where
/// the length is 3 or 4.
fn find_start_code(buf: &[u8], from: usize) -> Option<(usize, usize)> {
    let rel = buf.get(from..)?.windows(3).position(|w| w == [0, 0, 1])?;
    let j = from + rel;
    if j > from && buf[j - 1] == 0 {
        Some((j - 1, 4))
    } else {
        Some((j, 3))
    }
}

/// Wall-clock time in nanoseconds since the Unix epoch.
pub fn wall_clock_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Streaming SEI injector over an Annex-B byte stream.
///
/// `carry` holds only bytes that come AFTER the last processed start code:
/// either an unresolved start code still waiting for its NAL header byte, or
/// up to three trailing zeros that may open the next start code. A start code
/// whose NAL byte was already seen is never carried, so each VCL NAL gets
/// exactly one SEI however the stream is chunked.
#[derive(Debug, Default)]
pub struct SeiInjector {
    carry: Vec<u8>,
}

impl SeiInjector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed `chunk`; append everything that can be committed to `out`, with a
    /// SEI from `now_ns` in front of each VCL NAL found.
    pub fn push(&mut self, chunk: &[u8], mut now_ns: impl FnMut() -> u64, out: &mut Vec<u8>) {
        self.carry.extend_from_slice(chunk);
        let buf = &self.carry;
        let n = buf.len();
        let mut commit = 0usize;
        let mut scanned = 0usize;
        let mut unresolved: Option<usize> = None;
        while let Some((offset, len)) = find_start_code(buf, scanned) {
            let nal_idx = offset + len;
            if nal_idx >= n {
                // A start code with no NAL byte yet: decide on the next chunk.
                unresolved = Some(offset);
                break;
            }
            if is_vcl(buf[nal_idx]) {
                out.extend_from_slice(&buf[commit..offset]);
                out.extend_from_slice(&build_sei_nal(now_ns()));
                commit = offset;
            }
            scanned = nal_idx + 1;
        }
        let emit_until = match unresolved {
            Some(offset) => offset,
            None => {
                let tail = &buf[scanned..];
                let zeros = tail.iter().rev().take_while(|&&b| b == 0).count();
                n - zeros.min(MAX_HELD_ZEROS)
            }
        };
        out.extend_from_slice(&buf[commit..emit_until]);
        self.carry.drain(..emit_until);
    }

    /// Flush whatever is still held at end of stream.
    pub fn finish(&mut self, out: &mut Vec<u8>) {
        out.append(&mut self.carry);
    }
}

/// Copy `reader` to `writer`, injecting a wall-clock SEI before every VCL NAL.
/// Returns when either side closes or errors; the caller's process supervision
/// notices the ended pipeline.
pub async fn pump<R, W>(mut reader: R, mut writer: W)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut injector = SeiInjector::new();
    let mut chunk = vec![0u8; PUMP_CHUNK];
    let mut out: Vec<u8> = Vec::with_capacity(PUMP_CHUNK * 2);
    loop {
        let n = match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(error = %e, "sei_pump_read_failed");
                break;
            }
        };
        out.clear();
        injector.push(&chunk[..n], wall_clock_ns, &mut out);
        if !out.is_empty() {
            if let Err(e) = writer.write_all(&out).await {
                tracing::warn!(error = %e, "sei_pump_write_failed");
                return;
            }
            if let Err(e) = writer.flush().await {
                tracing::warn!(error = %e, "sei_pump_flush_failed");
                return;
            }
        }
    }
    out.clear();
    injector.finish(&mut out);
    if !out.is_empty() {
        let _ = writer.write_all(&out).await;
        let _ = writer.flush().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TS: u64 = 0x0102_0300_0000_0001;

    /// The SEI NAL is byte-identical to the predecessor's layout.
    #[test]
    fn sei_nal_layout_matches_the_parser() {
        let nal = build_sei_nal(0x1122_3344_5566_7788);
        let mut expected = vec![0, 0, 0, 1, 0x06, 0x05, 24];
        expected.extend_from_slice(&ADOS_LATENCY_SEI_UUID);
        expected.extend_from_slice(&0x1122_3344_5566_7788u64.to_be_bytes());
        expected.push(0x80);
        assert_eq!(nal, expected);
        // A timestamp with a `00 00 01` run is escaped, never a false start code.
        let escaped = build_sei_nal(0x0000_0100_0000_0000);
        assert!(find_start_code(&escaped, 4).is_none(), "{escaped:02x?}");
    }

    /// Deterministic xorshift so the fuzz needs no extra dependency.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// An Annex-B stream of random NALs: `(bytes, vcl_count)`. Payloads never
    /// contain two zeros in a row, so no false start code appears inside one.
    fn stream(rng: &mut Rng, nals: usize) -> (Vec<u8>, usize) {
        let mut out = Vec::new();
        let mut vcl = 0;
        for _ in 0..nals {
            if rng.below(2) == 0 {
                out.extend_from_slice(&[0, 0, 0, 1]);
            } else {
                out.extend_from_slice(&[0, 0, 1]);
            }
            let ty = [1u8, 5, 6, 7, 8, 9][rng.below(6) as usize];
            if is_vcl(ty) {
                vcl += 1;
            }
            out.push(0x60 | ty);
            let len = 1 + rng.below(40) as usize;
            let mut last_zero = false;
            for _ in 0..len {
                let mut b = rng.below(256) as u8;
                if b == 0 && last_zero {
                    b = 0x7f;
                }
                last_zero = b == 0;
                out.push(b);
            }
            // Never end a payload on a zero: it would merge with the next
            // start code into a different (still valid) split point.
            if last_zero {
                out.push(0x55);
            }
        }
        (out, vcl)
    }

    fn strip_sei(mut data: &[u8], sei: &[u8]) -> (Vec<u8>, usize) {
        let mut out = Vec::new();
        let mut count = 0;
        while !data.is_empty() {
            if data.starts_with(sei) {
                count += 1;
                data = &data[sei.len()..];
            } else {
                out.push(data[0]);
                data = &data[1..];
            }
        }
        (out, count)
    }

    #[test]
    fn sei_one_per_vcl_chunk_boundary_fuzz() {
        let sei = build_sei_nal(TS);
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for round in 0..200 {
            let nals = 1 + rng.below(30) as usize;
            let (input, vcl) = stream(&mut rng, nals);
            // Every chunk size from 1 byte up, plus random cuts.
            let mut plans: Vec<Vec<usize>> = (1..=8).map(|s| vec![s]).collect();
            plans.push((0..64).map(|_| 1 + rng.below(9) as usize).collect());
            for plan in plans {
                let mut inj = SeiInjector::new();
                let mut out = Vec::new();
                let mut at = 0;
                let mut step = 0;
                while at < input.len() {
                    let size = plan[step % plan.len()].min(input.len() - at);
                    inj.push(&input[at..at + size], || TS, &mut out);
                    at += size;
                    step += 1;
                }
                inj.finish(&mut out);
                let (restored, count) = strip_sei(&out, &sei);
                assert_eq!(count, vcl, "round {round} plan {plan:?}: one SEI per VCL");
                assert_eq!(restored, input, "round {round}: stream otherwise unchanged");
            }
        }
    }

    #[test]
    fn a_vcl_start_code_at_the_chunk_tail_gets_one_sei() {
        // The case that doubled the SEI before: start code + NAL byte are the
        // last four bytes of a chunk.
        let sei = build_sei_nal(TS);
        let mut inj = SeiInjector::new();
        let mut out = Vec::new();
        inj.push(&[0x09, 0xf0, 0, 0, 1, 0x65], || TS, &mut out);
        inj.push(&[0x88, 0x84, 0, 0, 0, 1, 0x41, 0x9a], || TS, &mut out);
        inj.finish(&mut out);
        let (restored, count) = strip_sei(&out, &sei);
        assert_eq!(count, 2);
        assert_eq!(
            restored,
            [0x09, 0xf0, 0, 0, 1, 0x65, 0x88, 0x84, 0, 0, 0, 1, 0x41, 0x9a]
        );
    }

    #[tokio::test]
    async fn pump_injects_between_reader_and_writer() {
        let input: Vec<u8> = [
            &[0u8, 0, 0, 1, 0x67, 0x42][..],
            &[0, 0, 1, 0x65, 0x11, 0x22],
        ]
        .concat();
        let mut out = Vec::new();
        pump(&input[..], &mut out).await;
        // One SEI (wall clock, so compare structure, not bytes). Its length
        // depends on how many emulation-prevention bytes the timestamp needs:
        // none for a timestamp with no zero bytes, the most for zero.
        let sei_len = out.len() - input.len();
        assert!(
            (build_sei_nal(u64::MAX).len()..=build_sei_nal(0).len()).contains(&sei_len),
            "{sei_len}"
        );
        assert!(out.ends_with(&[0, 0, 1, 0x65, 0x11, 0x22]));
        assert!(out.starts_with(&[0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x06]));
    }
}
