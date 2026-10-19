//! GNSS PPS samples for chrony's `SOCK` refclock driver.
//!
//! chrony reads `struct sock_sample` datagrams on the socket it binds for a
//! `refclock SOCK <path>` line (the installer writes
//! `refclock SOCK /run/chrony.ados.sock refid PPS ...`). Each datagram says: at
//! system time `tv` the true time was `tv + offset`. The struct is the
//! platform's C layout, native endian:
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
//! 40 bytes with the magic at 36 on a 64-bit host; the `timeval` widths (and
//! so the size) follow the target's `time_t` / `suseconds_t`.

use std::mem::{offset_of, size_of};
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};

use nix::libc::{c_int, suseconds_t, time_t, timeval};

/// The socket chrony binds for the refclock the installer configures.
pub const CHRONY_SOCK: &str = "/run/chrony.ados.sock";
/// `SOCK_MAGIC` from chrony's `refclock_sock.c`.
pub const SOCK_MAGIC: c_int = 0x534f_434b;

/// chrony's `struct sock_sample`; used only for its layout.
#[repr(C)]
#[allow(dead_code)]
struct SockSample {
    tv: timeval,
    offset: f64,
    pulse: c_int,
    leap: c_int,
    pad: c_int,
    magic: c_int,
}

/// `sizeof(struct sock_sample)` on this target.
pub const SAMPLE_LEN: usize = size_of::<SockSample>();
const TV_SEC_AT: usize = offset_of!(SockSample, tv) + offset_of!(timeval, tv_sec);
const TV_USEC_AT: usize = offset_of!(SockSample, tv) + offset_of!(timeval, tv_usec);
/// Byte offset of `magic`.
pub const MAGIC_AT: usize = offset_of!(SockSample, magic);

fn put(b: &mut [u8; SAMPLE_LEN], at: usize, bytes: &[u8]) {
    b[at..at + bytes.len()].copy_from_slice(bytes);
}

/// Encode one `struct sock_sample` (padding bytes zero).
pub fn encode_sample(
    tv_sec: i64,
    tv_usec: i64,
    offset_s: f64,
    pulse: i32,
    leap: i32,
) -> [u8; SAMPLE_LEN] {
    let mut b = [0u8; SAMPLE_LEN];
    put(&mut b, TV_SEC_AT, &(tv_sec as time_t).to_ne_bytes());
    put(&mut b, TV_USEC_AT, &(tv_usec as suseconds_t).to_ne_bytes());
    put(
        &mut b,
        offset_of!(SockSample, offset),
        &offset_s.to_ne_bytes(),
    );
    put(
        &mut b,
        offset_of!(SockSample, pulse),
        &(pulse as c_int).to_ne_bytes(),
    );
    put(
        &mut b,
        offset_of!(SockSample, leap),
        &(leap as c_int).to_ne_bytes(),
    );
    put(&mut b, MAGIC_AT, &SOCK_MAGIC.to_ne_bytes());
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

    /// `(tv_sec, tv_usec)` read back at the target's widths.
    fn tv(b: &[u8]) -> (i64, i64) {
        fn int_at(b: &[u8], at: usize, len: usize) -> i64 {
            match len {
                4 => i32::from_ne_bytes(b[at..at + 4].try_into().unwrap()) as i64,
                8 => i64::from_ne_bytes(b[at..at + 8].try_into().unwrap()),
                n => panic!("unexpected field width {n}"),
            }
        }
        (
            int_at(b, TV_SEC_AT, size_of::<time_t>()),
            int_at(b, TV_USEC_AT, size_of::<suseconds_t>()),
        )
    }

    fn offset(b: &[u8]) -> f64 {
        let at = offset_of!(SockSample, offset);
        f64::from_ne_bytes(b[at..at + 8].try_into().unwrap())
    }

    #[test]
    fn layout_follows_the_platform_timeval() {
        assert_eq!(SAMPLE_LEN, size_of::<SockSample>());
        assert_eq!(
            offset_of!(SockSample, offset),
            size_of::<timeval>().next_multiple_of(std::mem::align_of::<f64>())
        );
        assert_eq!(MAGIC_AT, SAMPLE_LEN - size_of::<c_int>());
        #[cfg(target_pointer_width = "64")]
        {
            assert_eq!(SAMPLE_LEN, 40);
            assert_eq!(MAGIC_AT, 36);
        }
        let b = encode_sample(1_760_860_000, 123_456, -0.25, 1, 2);
        assert_eq!(tv(&b), (1_760_860_000, 123_456));
        assert_eq!(offset(&b), -0.25);
        assert_eq!(i32_at(&b, offset_of!(SockSample, pulse)), 1);
        assert_eq!(i32_at(&b, offset_of!(SockSample, leap)), 2);
        assert_eq!(i32_at(&b, offset_of!(SockSample, pad)), 0);
        assert_eq!(i32_at(&b, MAGIC_AT), 0x534f434b);
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
        assert_eq!(i32_at(&b, offset_of!(SockSample, pulse)), 0);
        assert_eq!(i32_at(&b, offset_of!(SockSample, leap)), 0);
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
