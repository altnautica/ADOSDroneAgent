//! The `/run/ados/gnss.json` body (GET /api/v1/gnss minus `present`) and the
//! counters behind it.

use std::collections::VecDeque;
use std::path::Path;

use serde::Serialize;

use crate::base::BaseView;
use crate::receiver::{Fix, Rtk};
use crate::ubx::RelPosNed;

/// Schema version of `gnss.json`; mirrors the `gnss` sidecar row.
pub const GNSS_SIDECAR_VERSION: u16 = 1;
/// A heading report older than this reads as unavailable.
const HEADING_STALE_NS: u64 = 2_000_000_000;
/// Window of the RTCM message rate.
const RATE_WINDOW_NS: u64 = 2_000_000_000;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReceiverView {
    pub id: String,
    pub source: String,
    pub model: Option<String>,
    pub fix: Fix,
    pub sats: u8,
    pub hdop: Option<f64>,
    pub lat_deg: Option<f64>,
    pub lon_deg: Option<f64>,
    pub h_ellipsoid_m: Option<f64>,
    pub h_msl_m: Option<f64>,
    pub rtk: Rtk,
    pub pps_locked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct RtcmOutView {
    pub msgs_per_s: f64,
    pub bytes_total: u64,
    pub targets: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BaseOut {
    #[serde(flatten)]
    pub base: BaseView,
    pub rtcm_out: RtcmOutView,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct HeadingView {
    pub available: bool,
    pub deg: Option<f64>,
    pub accuracy_deg: Option<f64>,
    pub baseline_m: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GnssStatus {
    pub version: u16,
    pub receivers: Vec<ReceiverView>,
    pub base: BaseOut,
    pub heading: HeadingView,
}

/// RTCM3 frames forwarded from the base receiver.
#[derive(Debug, Default, Clone)]
pub struct RtcmCounter {
    times: VecDeque<u64>,
    bytes_total: u64,
}

impl RtcmCounter {
    pub fn record(&mut self, now: u64, len: usize) {
        self.bytes_total += len as u64;
        self.times.push_back(now);
        self.prune(now);
    }

    fn prune(&mut self, now: u64) {
        while self
            .times
            .front()
            .is_some_and(|t| now.saturating_sub(*t) > RATE_WINDOW_NS)
        {
            self.times.pop_front();
        }
    }

    pub fn view(&mut self, now: u64, targets: u32) -> RtcmOutView {
        self.prune(now);
        RtcmOutView {
            msgs_per_s: self.times.len() as f64 / (RATE_WINDOW_NS as f64 / 1e9),
            bytes_total: self.bytes_total,
            targets,
        }
    }
}

/// Moving-baseline heading from NAV-RELPOSNED.
#[derive(Debug, Default, Clone)]
pub struct Heading {
    last: Option<(u64, RelPosNed)>,
}

impl Heading {
    pub fn on_relposned(&mut self, now: u64, r: RelPosNed) {
        self.last = Some((now, r));
    }

    /// `enabled` is the config gate (two receivers and heading on).
    pub fn view(&self, now: u64, enabled: bool) -> HeadingView {
        let valid = self.last.filter(|(t, r)| {
            enabled
                && now.saturating_sub(*t) <= HEADING_STALE_NS
                && r.gnss_fix_ok
                && r.rel_pos_valid
                && r.heading_valid
        });
        match valid {
            Some((_, r)) => HeadingView {
                available: true,
                deg: Some(r.heading_deg.rem_euclid(360.0)),
                accuracy_deg: Some(r.acc_heading_deg),
                baseline_m: Some(r.length_m),
            },
            None => HeadingView {
                available: false,
                deg: None,
                accuracy_deg: None,
                baseline_m: None,
            },
        }
    }
}

/// Number of entries in the linked-peers sidecar (0 when absent).
pub fn linked_peer_count(path: &Path) -> u32 {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v.get("peers").and_then(|p| p.as_array()).map(Vec::len))
        .unwrap_or(0) as u32
}

/// Write `gnss.json` atomically.
pub fn write(path: &Path, status: &GnssStatus) -> std::io::Result<()> {
    ados_protocol::sidecar::write_json_atomic(path, status, 0o644)
}

#[cfg(test)]
mod tests;
