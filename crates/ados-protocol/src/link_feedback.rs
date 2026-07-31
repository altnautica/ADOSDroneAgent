//! Ground-measured video-link quality, reported back to the transmitting drone.
//!
//! ## Why this contract exists
//!
//! The drone's adaptive bitrate ladder steps on measured packet loss and RSSI.
//! On a drone those inputs are permanently absent: it transmits its own
//! downlink, and a single radio in monitor mode cannot capture its own injected
//! frames, so `packets_received` never leaves zero and every loss-driven
//! decision is gated off. The ladder therefore sat on its top rung for the whole
//! flight and could not shed rate on a link that was visibly over-fed.
//!
//! A local congestion signal (transmit-queue depth) covers only the case where
//! the offered rate exceeds what the radio will *accept*. It cannot see the case
//! that actually bites: `wfb_tx` accepts and injects everything, and the packets
//! are lost **in the air**, so the queue stays empty while a quarter of the
//! stream never arrives. The only honest measurement of that lives at the
//! receiver.
//!
//! So the receiver sends it back. The ground station already decodes the video
//! stream and counts exactly what was lost; this contract carries that count up
//! the existing aux uplink so the drone's ladder can act on a real sample
//! instead of a sentinel. No new radio, no new socket — one more channel on a
//! lane that already runs.
//!
//! ## Wire format
//!
//! Fixed 20-byte big-endian record. Fixed-size and not msgpack because this
//! rides a lossy radio lane at 1 Hz: a fixed record cannot be partially decoded
//! into a plausible-but-wrong sample, and its size is knowable before the read.
//!
//! ```text
//! offset  size  field
//!      0     1  version
//!      1     1  flags
//!      2     2  loss_percent      u16, hundredths (5.25% -> 525)
//!      4     2  rssi_dbm          i16, whole dBm
//!      6     2  snr_db            i16, tenths (12.4 dB -> 124)
//!      8     4  packets_received  u32, this interval
//!     12     4  fec_failed        u32, this interval
//!     16     4  bitrate_kbps      u32, delivered
//! ```
//!
//! Trailing bytes are IGNORED, not rejected, so a later build may append fields
//! without stranding an older peer mid-upgrade. A SHORT record is rejected: a
//! truncated sample would decode as artificially low loss, which would push the
//! ladder the wrong way on exactly the bad link that truncated it.

/// The framing version this build speaks.
pub const LINK_FEEDBACK_VERSION: u8 = 1;

/// Encoded size of a v1 record.
pub const LINK_FEEDBACK_LEN: usize = 20;

/// Set when the sender had a real decode this interval. Clear means "I am
/// listening and heard nothing", which is a materially different statement from
/// "I heard a clean link" and must never be folded into one.
pub const FLAG_HAS_MEASUREMENT: u8 = 0b0000_0001;

/// A ground-measured sample of the drone's downlink.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LinkFeedback {
    /// Percent of expected packets that did not arrive, 0.0–100.0.
    pub loss_percent: f64,
    /// Received signal strength, whole dBm (negative).
    pub rssi_dbm: f64,
    /// Signal-to-noise ratio in dB.
    pub snr_db: f64,
    /// Packets decoded this interval. Zero with [`FLAG_HAS_MEASUREMENT`] clear
    /// means the receiver heard nothing at all.
    pub packets_received: u32,
    /// FEC blocks that could not be recovered this interval. This is the
    /// counter that distinguishes a busy-but-fine link from a lossy one, and
    /// the one the local transmit queue can never see.
    pub fec_failed: u32,
    /// Delivered application bitrate in kbps, as measured at the receiver.
    pub bitrate_kbps: u32,
    /// Whether the sender actually decoded anything this interval.
    pub has_measurement: bool,
}

/// Why a record could not be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkFeedbackError {
    /// Fewer than [`LINK_FEEDBACK_LEN`] bytes.
    TooShort(usize),
    /// A version this build does not speak.
    UnsupportedVersion(u8),
}

impl LinkFeedback {
    /// Encode as a v1 record.
    ///
    /// Saturating conversions throughout: a nonsense input (a wild RSSI from a
    /// driver glitch, a loss above 100) clamps to the representable edge rather
    /// than wrapping into a value of the opposite meaning.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(LINK_FEEDBACK_LEN);
        out.push(LINK_FEEDBACK_VERSION);
        out.push(if self.has_measurement {
            FLAG_HAS_MEASUREMENT
        } else {
            0
        });
        let loss = (self.loss_percent.clamp(0.0, 100.0) * 100.0).round() as u16;
        out.extend_from_slice(&loss.to_be_bytes());
        let rssi = self.rssi_dbm.clamp(-32768.0, 32767.0).round() as i16;
        out.extend_from_slice(&rssi.to_be_bytes());
        let snr = (self.snr_db.clamp(-3276.8, 3276.7) * 10.0).round() as i16;
        out.extend_from_slice(&snr.to_be_bytes());
        out.extend_from_slice(&self.packets_received.to_be_bytes());
        out.extend_from_slice(&self.fec_failed.to_be_bytes());
        out.extend_from_slice(&self.bitrate_kbps.to_be_bytes());
        out
    }

    /// Decode a v1 record. Trailing bytes are ignored; a short record is an
    /// error.
    pub fn decode(buf: &[u8]) -> Result<Self, LinkFeedbackError> {
        if buf.len() < LINK_FEEDBACK_LEN {
            return Err(LinkFeedbackError::TooShort(buf.len()));
        }
        if buf[0] != LINK_FEEDBACK_VERSION {
            return Err(LinkFeedbackError::UnsupportedVersion(buf[0]));
        }
        let u16at = |i: usize| u16::from_be_bytes([buf[i], buf[i + 1]]);
        let i16at = |i: usize| i16::from_be_bytes([buf[i], buf[i + 1]]);
        let u32at = |i: usize| u32::from_be_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]);
        Ok(Self {
            has_measurement: buf[1] & FLAG_HAS_MEASUREMENT != 0,
            loss_percent: f64::from(u16at(2)) / 100.0,
            rssi_dbm: f64::from(i16at(4)),
            snr_db: f64::from(i16at(6)) / 10.0,
            packets_received: u32at(8),
            fec_failed: u32at(12),
            bitrate_kbps: u32at(16),
        })
    }
}

/// Where the drone publishes the last feedback record it received.
///
/// The receiving process (the MAVLink router, which owns the aux uplink) and
/// the consuming process (the radio service, which owns the bitrate ladder) are
/// separate, so the sample crosses via a sidecar file rather than a socket —
/// the same shape the encoder-ceiling reconcile already uses, and readable by
/// an operator running `cat` during a bench session.
pub const LINK_FEEDBACK_SIDECAR: &str = "/run/ados/link-feedback.json";

/// The sidecar path, honouring the `ADOS_RUN_DIR` override so a test (and a
/// sim-bench run) can point the writer and the reader at the same temp dir
/// rather than the real `/run/ados`.
pub fn sidecar_path() -> std::path::PathBuf {
    match std::env::var("ADOS_RUN_DIR") {
        Ok(dir) if !dir.trim().is_empty() => {
            std::path::PathBuf::from(dir).join("link-feedback.json")
        }
        _ => std::path::PathBuf::from(LINK_FEEDBACK_SIDECAR),
    }
}

/// How long a received sample stays usable.
///
/// A stale sample is worse than none: the ladder would hold a rate chosen for a
/// link state that has since changed, and the most likely reason feedback
/// stopped arriving is that the link got WORSE — so treating an old sample as
/// current would hold the rate high exactly when it should fall. Three missed
/// 1 Hz reports is the trip.
pub const FEEDBACK_STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(3);

/// The on-disk form: the received record plus when it arrived.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LinkFeedbackSidecar {
    pub version: u8,
    /// Wall-clock milliseconds since the epoch when this record was received.
    pub received_at_unix_ms: u64,
    pub loss_percent: f64,
    pub rssi_dbm: f64,
    pub snr_db: f64,
    pub packets_received: u32,
    pub fec_failed: u32,
    pub bitrate_kbps: u32,
    pub has_measurement: bool,
}

impl LinkFeedbackSidecar {
    /// Stamp a received record with the current wall clock.
    pub fn now(fb: &LinkFeedback) -> Self {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Self::stamped(fb, ms)
    }

    /// Stamp with an explicit time (the seam the tests drive).
    pub fn stamped(fb: &LinkFeedback, received_at_unix_ms: u64) -> Self {
        Self {
            version: LINK_FEEDBACK_VERSION,
            received_at_unix_ms,
            loss_percent: fb.loss_percent,
            rssi_dbm: fb.rssi_dbm,
            snr_db: fb.snr_db,
            packets_received: fb.packets_received,
            fec_failed: fb.fec_failed,
            bitrate_kbps: fb.bitrate_kbps,
            has_measurement: fb.has_measurement,
        }
    }

    /// Whether this record is recent enough to act on, given a current epoch-ms
    /// reading. A record stamped in the future (a clock step) is treated as
    /// stale, not as infinitely fresh.
    pub fn is_fresh_at(&self, now_unix_ms: u64) -> bool {
        let age = now_unix_ms.saturating_sub(self.received_at_unix_ms);
        now_unix_ms >= self.received_at_unix_ms && age <= FEEDBACK_STALE_AFTER.as_millis() as u64
    }

    /// Whether this record is a usable ladder input right now: fresh AND an
    /// actual measurement. Both gates in one place so no caller can apply one
    /// and forget the other.
    pub fn is_usable_at(&self, now_unix_ms: u64) -> bool {
        self.has_measurement && self.is_fresh_at(now_unix_ms)
    }
}

/// Read the sidecar, or `None` when it is absent or unparseable.
pub fn read_sidecar_from(path: &std::path::Path) -> Option<LinkFeedbackSidecar> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Write the sidecar via a temp file and rename, so a reader mid-tick never
/// observes a half-written record.
pub fn write_sidecar_to(path: &std::path::Path, s: &LinkFeedbackSidecar) -> std::io::Result<()> {
    let body = serde_json::to_vec(s)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &body)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> LinkFeedback {
        LinkFeedback {
            loss_percent: 24.29,
            rssi_dbm: -36.0,
            snr_db: 12.4,
            packets_received: 485,
            fec_failed: 25,
            bitrate_kbps: 2242,
            has_measurement: true,
        }
    }

    #[test]
    fn round_trips_a_real_sample() {
        let s = sample();
        let got = LinkFeedback::decode(&s.encode()).expect("decode");
        assert!((got.loss_percent - 24.29).abs() < 0.01);
        assert_eq!(got.rssi_dbm, -36.0);
        assert!((got.snr_db - 12.4).abs() < 0.05);
        assert_eq!(got.packets_received, 485);
        assert_eq!(got.fec_failed, 25);
        assert_eq!(got.bitrate_kbps, 2242);
        assert!(got.has_measurement);
    }

    #[test]
    fn encodes_to_the_declared_length() {
        assert_eq!(sample().encode().len(), LINK_FEEDBACK_LEN);
    }

    #[test]
    fn heard_nothing_is_distinct_from_a_clean_link() {
        // Both have zero loss and zero packets; only the flag separates them,
        // and conflating them would let a deaf receiver read as a perfect link.
        let deaf = LinkFeedback {
            has_measurement: false,
            packets_received: 0,
            loss_percent: 0.0,
            ..sample()
        };
        let decoded = LinkFeedback::decode(&deaf.encode()).expect("decode");
        assert!(!decoded.has_measurement);
    }

    #[test]
    fn a_short_record_is_rejected_not_zero_filled() {
        // A truncated record would decode as artificially LOW loss and push the
        // ladder up on exactly the bad link that truncated it.
        let full = sample().encode();
        for n in 0..LINK_FEEDBACK_LEN {
            assert_eq!(
                LinkFeedback::decode(&full[..n]),
                Err(LinkFeedbackError::TooShort(n)),
                "len {n} must be rejected"
            );
        }
    }

    #[test]
    fn trailing_bytes_are_ignored_so_a_newer_peer_still_decodes() {
        let mut buf = sample().encode();
        buf.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(LinkFeedback::decode(&buf).expect("decode"), sample());
    }

    #[test]
    fn an_unknown_version_is_rejected() {
        let mut buf = sample().encode();
        buf[0] = 0x7F;
        assert_eq!(
            LinkFeedback::decode(&buf),
            Err(LinkFeedbackError::UnsupportedVersion(0x7F))
        );
    }

    #[test]
    fn a_fresh_measurement_is_usable() {
        let s = LinkFeedbackSidecar::stamped(&sample(), 10_000);
        assert!(s.is_usable_at(10_500));
        assert!(s.is_usable_at(13_000), "3s is the edge, still usable");
    }

    #[test]
    fn a_stale_record_is_not_usable() {
        // The failure this guards: feedback stops because the link got WORSE,
        // and holding the last good sample would keep the rate high exactly
        // when it should fall.
        let s = LinkFeedbackSidecar::stamped(&sample(), 10_000);
        assert!(!s.is_usable_at(13_001));
        assert!(!s.is_usable_at(60_000));
    }

    #[test]
    fn a_record_stamped_in_the_future_is_stale_not_infinitely_fresh() {
        let s = LinkFeedbackSidecar::stamped(&sample(), 50_000);
        assert!(!s.is_usable_at(10_000));
    }

    #[test]
    fn a_fresh_non_measurement_is_not_usable() {
        let deaf = LinkFeedback {
            has_measurement: false,
            ..sample()
        };
        let s = LinkFeedbackSidecar::stamped(&deaf, 10_000);
        assert!(s.is_fresh_at(10_100), "it did arrive recently");
        assert!(
            !s.is_usable_at(10_100),
            "but a receiver that heard nothing is not a loss measurement"
        );
    }

    #[test]
    fn the_sidecar_round_trips_through_a_file() {
        let dir = std::env::temp_dir().join(format!("ados-lf-{}", std::process::id()));
        let path = dir.join("link-feedback.json");
        let s = LinkFeedbackSidecar::stamped(&sample(), 1234);
        write_sidecar_to(&path, &s).expect("write");
        let back = read_sidecar_from(&path).expect("read");
        assert_eq!(back.received_at_unix_ms, 1234);
        assert!((back.loss_percent - 24.29).abs() < 0.001);
        assert!(back.has_measurement);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_sidecar_reads_as_none_rather_than_a_default() {
        let missing = std::path::Path::new("/nonexistent/ados/link-feedback.json");
        assert!(read_sidecar_from(missing).is_none());
    }

    #[test]
    fn out_of_range_inputs_clamp_rather_than_wrap() {
        let wild = LinkFeedback {
            loss_percent: 900.0,
            rssi_dbm: -99999.0,
            snr_db: 1e9,
            ..sample()
        };
        let got = LinkFeedback::decode(&wild.encode()).expect("decode");
        assert_eq!(got.loss_percent, 100.0, "loss clamps to 100, never wraps");
        assert!(
            got.rssi_dbm <= 0.0,
            "rssi stays negative, never wraps positive"
        );
        assert!(got.snr_db > 0.0, "snr stays positive, never wraps negative");
    }
}
