//! GNSS PPS samples for chrony's `SOCK` refclock driver.
//!
//! chrony reads `struct sock_sample` datagrams on the socket it binds for a
//! `refclock SOCK <path>` line (the installer writes
//! `refclock SOCK /run/chrony.ados.sock refid PPS ...`). Each datagram says: at
//! system time `tv` the true time was `tv + offset`. chrony's definition
//! (`refclock_sock.c`):
//!
//! ```c
//! struct sock_sample {
//!     struct timeval tv;   /* time_t tv_sec; suseconds_t tv_usec */
//!     double offset;       /* seconds */
//!     int pulse;
//!     int leap;
//!     int _pad;
//!     int magic;           /* 0x534f434b */
//! };
//! ```
//!
//! chrony drops a datagram whose length is not `sizeof(struct sock_sample)`
//! as *its* build sees it, so the layout to emit is the one chrony was
//! compiled with, not this binary's libc. Every 64-bit Linux and every 32-bit
//! distro built with 64-bit `time_t` (glibc `_TIME_BITS=64`: Debian 13 and
//! Ubuntu 24.04 armhf after the t64 transition, where `struct timeval` becomes
//! `__time64_t` + `__suseconds64_t`) gives the same 40 bytes, native endian:
//!
//! | offset | type | field     |
//! |--------|------|-----------|
//! | 0      | i64  | tv_sec    |
//! | 8      | i64  | tv_usec   |
//! | 16     | f64  | offset, s |
//! | 24     | i32  | pulse     |
//! | 28     | i32  | leap      |
//! | 32     | i32  | _pad      |
//! | 36     | i32  | magic     |
//!
//! That layout is written explicitly, whatever the target's own `time_t`. A
//! chrony built with 32-bit `time_t` (pre-t64 armhf, e.g. Debian 12) expects
//! 32 bytes and rejects these samples; the shipped binaries are aarch64-only,
//! where the chrony beside them is 64-bit.

use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};

/// The socket chrony binds for the refclock the installer configures.
pub const CHRONY_SOCK: &str = "/run/chrony.ados.sock";
/// `SOCK_MAGIC` from chrony's `refclock_sock.c`.
pub const SOCK_MAGIC: i32 = 0x534f_434b;
/// `sizeof(struct sock_sample)` with a 64-bit `struct timeval`.
pub const SAMPLE_LEN: usize = 40;
const TV_SEC_AT: usize = 0;
const TV_USEC_AT: usize = 8;
const OFFSET_AT: usize = 16;
const PULSE_AT: usize = 24;
const LEAP_AT: usize = 28;
const PAD_AT: usize = 32;
/// Byte offset of `magic`.
pub const MAGIC_AT: usize = 36;

/// Encode one `struct sock_sample` (padding zero).
pub fn encode_sample(
    tv_sec: i64,
    tv_usec: i64,
    offset_s: f64,
    pulse: i32,
    leap: i32,
) -> [u8; SAMPLE_LEN] {
    let mut b = [0u8; SAMPLE_LEN];
    b[TV_SEC_AT..TV_USEC_AT].copy_from_slice(&tv_sec.to_ne_bytes());
    b[TV_USEC_AT..OFFSET_AT].copy_from_slice(&tv_usec.to_ne_bytes());
    b[OFFSET_AT..PULSE_AT].copy_from_slice(&offset_s.to_ne_bytes());
    b[PULSE_AT..LEAP_AT].copy_from_slice(&pulse.to_ne_bytes());
    b[LEAP_AT..PAD_AT].copy_from_slice(&leap.to_ne_bytes());
    b[MAGIC_AT..SAMPLE_LEN].copy_from_slice(&SOCK_MAGIC.to_ne_bytes());
    b
}

/// The sample for one pulse: at host monotonic `host_ns` the true UTC was
/// `utc_ns`. `realtime_minus_mono_ns` (CLOCK_REALTIME − CLOCK_MONOTONIC, read
/// now) maps the pulse onto the system clock; the offset is measured from the
/// microsecond-truncated `tv` actually sent.
pub fn pulse_sample(host_ns: u64, utc_ns: i64, realtime_minus_mono_ns: i64) -> [u8; SAMPLE_LEN] {
    const NS: i64 = 1_000_000_000;
    let tv_ns = host_ns as i64 + realtime_minus_mono_ns;
    let sec = tv_ns.div_euclid(NS);
    let usec = tv_ns.rem_euclid(NS) / 1_000;
    let sent_ns = sec * NS + usec * 1_000;
    let offset_s = (utc_ns - sent_ns) as f64 / 1e9;
    encode_sample(sec, usec, offset_s, 0, 0)
}

/// Sends samples to chrony's socket. The socket is non-blocking, so a send
/// never stalls the caller: an absent or full receiver drops the sample.
#[derive(Debug)]
pub struct ChronySink {
    path: PathBuf,
    sock: Option<UnixDatagram>,
    sent: u64,
}

impl ChronySink {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        ChronySink {
            path: path.into(),
            sock: None,
            sent: 0,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Samples delivered so far.
    pub fn sent(&self) -> u64 {
        self.sent
    }

    /// Send one sample. Returns false when chrony's socket is absent (chrony
    /// not running, refclock not configured) or the send would block.
    pub fn send(&mut self, sample: &[u8; SAMPLE_LEN]) -> bool {
        if self.sock.is_none() {
            match UnixDatagram::unbound().and_then(|s| s.set_nonblocking(true).map(|()| s)) {
                Ok(s) => self.sock = Some(s),
                Err(e) => {
                    tracing::debug!(error = %e, "chrony socket create failed");
                    return false;
                }
            }
        }
        let Some(sock) = &self.sock else {
            return false;
        };
        match sock.send_to(sample, &self.path) {
            Ok(_) => {
                self.sent += 1;
                true
            }
            Err(e) => {
                tracing::debug!(error = %e, path = %self.path.display(), "chrony sample not sent");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn i32_at(b: &[u8], at: usize) -> i32 {
        i32::from_ne_bytes(b[at..at + 4].try_into().unwrap())
    }

    fn tv(b: &[u8]) -> (i64, i64) {
        (
            i64::from_ne_bytes(b[0..8].try_into().unwrap()),
            i64::from_ne_bytes(b[8..16].try_into().unwrap()),
        )
    }

    fn offset(b: &[u8]) -> f64 {
        f64::from_ne_bytes(b[16..24].try_into().unwrap())
    }

    /// The exact bytes chrony's 64-bit-`time_t` build reads, identical on
    /// every target (no dependence on this target's libc `timeval`).
    #[test]
    fn layout_is_chronys_64_bit_timeval_on_every_target() {
        let b = encode_sample(1_760_860_000, 123_456, -0.25, 1, 2);
        let mut want = Vec::with_capacity(40);
        want.extend(1_760_860_000i64.to_ne_bytes());
        want.extend(123_456i64.to_ne_bytes());
        want.extend((-0.25f64).to_ne_bytes());
        want.extend(1i32.to_ne_bytes());
        want.extend(2i32.to_ne_bytes());
        want.extend(0i32.to_ne_bytes());
        want.extend(0x534f_434bi32.to_ne_bytes());
        assert_eq!(b.as_slice(), want.as_slice());
        assert_eq!(SAMPLE_LEN, 40);
        assert_eq!(MAGIC_AT, 36);
        // The C layout chrony builds on a 64-bit-time_t system, mirrored.
        #[repr(C)]
        struct Sample64 {
            tv_sec: i64,
            tv_usec: i64,
            offset: f64,
            pulse: i32,
            leap: i32,
            pad: i32,
            magic: i32,
        }
        assert_eq!(std::mem::size_of::<Sample64>(), SAMPLE_LEN);
        assert_eq!(std::mem::offset_of!(Sample64, magic), MAGIC_AT);
        let s = Sample64 {
            tv_sec: 0,
            tv_usec: 0,
            offset: 0.0,
            pulse: 0,
            leap: 0,
            pad: 0,
            magic: 0,
        };
        let _ = (
            s.tv_sec, s.tv_usec, s.offset, s.pulse, s.leap, s.pad, s.magic,
        );
    }

    #[test]
    fn pulse_maps_onto_the_system_clock() {
        // Host mono 100.000_000_500 s, realtime runs 1.76e9 s ahead, and the
        // system clock is 2 ms slow against the true UTC.
        let host = 100_000_000_500u64;
        let delta = 1_760_000_000_000_000_000i64;
        let utc = host as i64 + delta + 2_000_000;
        let b = pulse_sample(host, utc, delta);
        assert_eq!(tv(&b), (1_760_000_100, 0));
        // 2 ms plus the 500 ns the microsecond truncation dropped.
        assert!((offset(&b) - 0.002_000_5).abs() < 1e-9, "{}", offset(&b));
        assert_eq!(i32_at(&b, PULSE_AT), 0);
        assert_eq!(i32_at(&b, LEAP_AT), 0);
    }

    #[test]
    fn delivers_to_a_bound_socket_and_skips_an_absent_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chrony.sock");
        let mut sink = ChronySink::new(&path);
        assert!(!sink.send(&encode_sample(1, 2, 0.0, 0, 0)));
        let rx = UnixDatagram::bind(&path).unwrap();
        assert!(sink.send(&encode_sample(1, 2, 0.5, 0, 0)));
        let mut buf = [0u8; 64];
        let n = rx.recv(&mut buf).unwrap();
        assert_eq!(n, SAMPLE_LEN);
        assert_eq!(i32_at(&buf, MAGIC_AT), SOCK_MAGIC);
        assert_eq!(sink.sent(), 1);
    }
}
