//! GNSS PPS samples for chrony's `SOCK` refclock driver.
//!
//! chrony reads `struct sock_sample` datagrams on the socket it binds for a
//! `refclock SOCK <path>` line (the installer writes
//! `refclock SOCK /run/chrony.ados.sock refid PPS ...`). Each datagram says: at
//! system time `tv` the true time was `tv + offset`. Fields are native endian:
//!
//! | offset | type             | field      |
//! |--------|------------------|------------|
//! | 0      | i64              | tv_sec     |
//! | 8      | i64              | tv_usec    |
//! | 16     | f64              | offset (s) |
//! | 24     | i32              | pulse      |
//! | 28     | i32              | leap       |
//! | 32     | i32              | padding    |
//! | 36     | i32              | magic      |

use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};

/// The socket chrony binds for the refclock the installer configures.
pub const CHRONY_SOCK: &str = "/run/chrony.ados.sock";
/// `SOCK_MAGIC` from chrony's `refclock_sock.c`.
pub const SOCK_MAGIC: i32 = 0x534f_434b;
/// `sizeof(struct sock_sample)` on a 64-bit host.
pub const SAMPLE_LEN: usize = 40;

/// Encode one `struct sock_sample`.
pub fn encode_sample(
    tv_sec: i64,
    tv_usec: i64,
    offset_s: f64,
    pulse: i32,
    leap: i32,
) -> [u8; SAMPLE_LEN] {
    let mut b = [0u8; SAMPLE_LEN];
    b[0..8].copy_from_slice(&tv_sec.to_ne_bytes());
    b[8..16].copy_from_slice(&tv_usec.to_ne_bytes());
    b[16..24].copy_from_slice(&offset_s.to_ne_bytes());
    b[24..28].copy_from_slice(&pulse.to_ne_bytes());
    b[28..32].copy_from_slice(&leap.to_ne_bytes());
    b[32..36].copy_from_slice(&0i32.to_ne_bytes());
    b[36..40].copy_from_slice(&SOCK_MAGIC.to_ne_bytes());
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

/// Sends samples to chrony's socket when it exists. Never blocks.
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

    /// Send one sample. Returns false when chrony's socket is absent or the
    /// send failed (chrony not running, refclock not configured).
    pub fn send(&mut self, sample: &[u8; SAMPLE_LEN]) -> bool {
        if !self.path.exists() {
            return false;
        }
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
                tracing::debug!(error = %e, path = %self.path.display(), "chrony sample send failed");
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

    fn i64_at(b: &[u8], at: usize) -> i64 {
        i64::from_ne_bytes(b[at..at + 8].try_into().unwrap())
    }

    #[test]
    fn layout_is_forty_bytes_with_the_magic_last() {
        let b = encode_sample(1_760_860_000, 123_456, -0.25, 1, 2);
        assert_eq!(b.len(), 40);
        assert_eq!(i64_at(&b, 0), 1_760_860_000);
        assert_eq!(i64_at(&b, 8), 123_456);
        assert_eq!(f64::from_ne_bytes(b[16..24].try_into().unwrap()), -0.25);
        assert_eq!(i32_at(&b, 24), 1);
        assert_eq!(i32_at(&b, 28), 2);
        assert_eq!(i32_at(&b, 32), 0);
        assert_eq!(i32_at(&b, 36), 0x534f434b);
    }

    #[test]
    fn pulse_maps_onto_the_system_clock() {
        // Host mono 100.000_000_500 s, realtime runs 1.76e9 s ahead, and the
        // system clock is 2 ms slow against the true UTC.
        let host = 100_000_000_500u64;
        let delta = 1_760_000_000_000_000_000i64;
        let utc = host as i64 + delta + 2_000_000;
        let b = pulse_sample(host, utc, delta);
        assert_eq!(i64_at(&b, 0), 1_760_000_100);
        assert_eq!(i64_at(&b, 8), 0);
        let off = f64::from_ne_bytes(b[16..24].try_into().unwrap());
        // 2 ms plus the 500 ns the microsecond truncation dropped.
        assert!((off - 0.002_000_5).abs() < 1e-9, "{off}");
        assert_eq!(i32_at(&b, 24), 0);
        assert_eq!(i32_at(&b, 28), 0);
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
        assert_eq!(i32_at(&buf, 36), SOCK_MAGIC);
        assert_eq!(sink.sent(), 1);
    }
}
