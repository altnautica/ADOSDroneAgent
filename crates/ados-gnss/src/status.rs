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

/// A linked peer counts while its last decoded beacon is at most this old
/// (seconds): the radio's peer-stale threshold, after which the drone has
/// given the link up. The sidecar has no single window; each entry carries its
/// own `last_seen_unix`.
pub const LINKED_PEER_FRESH_S: f64 = 25.0;

/// Number of linked peers whose own `last_seen_unix` is fresh (0 when the
/// sidecar is absent or unreadable).
pub fn linked_peer_count(path: &Path) -> u32 {
    let now_unix = ados_sensorbus::time::realtime_ns() as f64 / 1e9;
    linked_peer_count_at(path, now_unix)
}

/// [`linked_peer_count`] at wall time `now_unix` (seconds). An entry without
/// a readable `last_seen_unix`, or one from the future, does not count.
pub fn linked_peer_count_at(path: &Path, now_unix: f64) -> u32 {
    let Some(doc) = std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
    else {
        return 0;
    };
    let Some(peers) = doc.get("peers").and_then(|p| p.as_array()) else {
        return 0;
    };
    peers
        .iter()
        .filter_map(|p| p.get("last_seen_unix").and_then(serde_json::Value::as_f64))
        .filter(|seen| {
            let age = now_unix - seen;
            (0.0..=LINKED_PEER_FRESH_S).contains(&age)
        })
        .count() as u32
}

/// Write `gnss.json` atomically.
pub fn write(path: &Path, status: &GnssStatus) -> std::io::Result<()> {
    ados_protocol::sidecar::write_json_atomic(path, status, 0o644)
}

#[cfg(test)]
mod tests;
