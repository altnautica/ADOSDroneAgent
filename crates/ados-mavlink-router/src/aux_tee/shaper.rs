//! Frame classification and the byte budget that sheds bulk first.

use std::time::Instant;

use super::{
    Priority, RateShaper, ShaperConfig, CRITICAL_MESSAGE_IDS, DEFAULT_BUDGET_BYTES_PER_SEC,
    DEFAULT_BULK_RESERVE_BYTES, DEFAULT_BURST_BYTES, STX_V1, STX_V2,
};

/// Read the message id out of a raw MAVLink frame without decoding it.
///
/// Deliberately a header read rather than a full parse: it costs nothing per
/// frame and, more importantly, it works for messages this build's dialect does
/// not know. A full parse would fail on an unknown message and misreport it,
/// which on a shed decision means silently mis-prioritising real traffic.
///
/// Returns `None` for anything that is not a recognisable frame header.
pub fn mavlink_message_id(frame: &[u8]) -> Option<u32> {
    match frame.first().copied()? {
        // v2: magic, length, incompat, compat, sequence, system, component,
        // then a 24-bit little-endian message id at bytes 7 through 9.
        STX_V2 if frame.len() >= 10 => {
            Some(u32::from(frame[7]) | (u32::from(frame[8]) << 8) | (u32::from(frame[9]) << 16))
        }
        // v1: magic, length, sequence, system, component, then a 1-byte id.
        STX_V1 if frame.len() >= 6 => Some(u32::from(frame[5])),
        _ => None,
    }
}

/// Classify a frame for shedding. An unreadable header is bulk: an
/// unrecognisable frame is the last thing worth spending a congested lane on.
pub fn frame_priority(frame: &[u8]) -> Priority {
    match mavlink_message_id(frame) {
        Some(id) if CRITICAL_MESSAGE_IDS.contains(&id) => Priority::Critical,
        _ => Priority::Bulk,
    }
}

impl Default for ShaperConfig {
    fn default() -> Self {
        Self {
            budget_bytes_per_sec: DEFAULT_BUDGET_BYTES_PER_SEC,
            burst_bytes: DEFAULT_BURST_BYTES,
            bulk_reserve_bytes: DEFAULT_BULK_RESERVE_BYTES,
        }
    }
}

impl RateShaper {
    /// A shaper starting with a full bucket, as of now.
    pub fn new(cfg: ShaperConfig) -> Self {
        Self::starting_at(cfg, Instant::now())
    }

    /// A shaper starting with a full bucket at an explicit instant, so a test
    /// can drive time by hand instead of sleeping.
    pub fn starting_at(cfg: ShaperConfig, now: Instant) -> Self {
        let cfg = ShaperConfig {
            budget_bytes_per_sec: cfg.budget_bytes_per_sec.max(0.0),
            burst_bytes: cfg.burst_bytes.max(0.0),
            // A reserve above the bucket size would starve bulk outright; a
            // negative one would be meaningless. Neither is reachable from the
            // shipped defaults, so this is only defensive.
            bulk_reserve_bytes: cfg.bulk_reserve_bytes.clamp(0.0, cfg.burst_bytes.max(0.0)),
        };
        Self {
            tokens: cfg.burst_bytes,
            cfg,
            last: now,
        }
    }

    /// Tokens currently available.
    pub fn tokens(&self) -> f64 {
        self.tokens
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        if elapsed <= 0.0 {
            return;
        }
        self.last = now;
        self.tokens =
            (self.tokens + elapsed * self.cfg.budget_bytes_per_sec).min(self.cfg.burst_bytes);
    }

    /// Whether a frame of `cost` on-air bytes may be sent now, spending its
    /// tokens if so. A refusal spends nothing.
    pub fn admit(&mut self, now: Instant, cost: usize, priority: Priority) -> bool {
        self.refill(now);
        let floor = match priority {
            Priority::Critical => 0.0,
            Priority::Bulk => self.cfg.bulk_reserve_bytes,
        };
        let cost = cost as f64;
        if self.tokens - cost >= floor {
            self.tokens -= cost;
            true
        } else {
            false
        }
    }
}
