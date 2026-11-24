//! The in-process link-quality history ring behind `GET /api/wfb/history`.
//!
//! The store's `link.*` aggregate is the durable history, but the store is off
//! by default, and a ground station's receiver never emits those metrics, so
//! the route answered an empty history on a healthy link. A sampler reads the
//! same `wfb-stats.json` sidecar `GET /api/wfb` reads, every
//! [`SAMPLE_PERIOD`], and keeps [`RING_SECONDS`] of samples in memory. The
//! route serves the store when it has data, else this ring.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use ados_protocol::wfb_status::{
    build_status_from_stats_file_at, now_unix_micros, WfbStatusConfig,
};
use serde_json::{json, Map, Value};

/// How often the sampler reads the sidecar.
pub(crate) const SAMPLE_PERIOD: Duration = Duration::from_secs(2);
/// The longest window the route serves (its `?seconds=` clamp).
const RING_SECONDS: i64 = 300;
/// The sample fields, in the route's sample shape.
const FIELDS: [&str; 4] = ["rssi_dbm", "snr_db", "loss_percent", "bitrate_kbps"];

/// A bounded, time-ordered ring of link samples.
#[derive(Debug, Default)]
pub(crate) struct LinkHistory {
    samples: VecDeque<(i64, Map<String, Value>)>,
}

impl LinkHistory {
    /// Append one sample from a `GET /api/wfb` status body. A stale or
    /// silent status (no decoded packets) records nothing, so the trace never
    /// shows a no-measurement sentinel as a reading.
    pub(crate) fn push(&mut self, ts_us: i64, status: &Value) {
        let stale = status.get("state").and_then(Value::as_str) == Some("stale");
        let received = status
            .get("packets_received")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        if stale || received <= 0.0 {
            self.prune(ts_us);
            return;
        }
        let mut sample = Map::new();
        for key in FIELDS {
            sample.insert(
                key.to_string(),
                status.get(key).cloned().unwrap_or(Value::Null),
            );
        }
        self.samples.push_back((ts_us, sample));
        self.prune(ts_us);
    }

    fn prune(&mut self, now_us: i64) {
        let floor = now_us - RING_SECONDS * 1_000_000;
        while self.samples.front().is_some_and(|(ts, _)| *ts < floor) {
            self.samples.pop_front();
        }
    }

    /// The samples of the last `seconds` before `now_us` in the route's
    /// `{samples, count}` shape.
    pub(crate) fn window(&self, now_us: i64, seconds: i64) -> Value {
        let floor = now_us - seconds * 1_000_000;
        let samples: Vec<Value> = self
            .samples
            .iter()
            .filter(|(ts, _)| *ts >= floor)
            .map(|(ts, vals)| {
                let mut obj = Map::new();
                obj.insert("timestamp".to_string(), json!(super::wfb::iso_from_us(*ts)));
                obj.extend(vals.clone());
                Value::Object(obj)
            })
            .collect();
        let count = samples.len();
        json!({"samples": samples, "count": count})
    }
}

/// The process-wide ring the sampler fills and the route reads.
pub(crate) static RING: LazyLock<Mutex<LinkHistory>> =
    LazyLock::new(|| Mutex::new(LinkHistory::default()));

/// The ring's window, for the route.
pub(crate) fn ring_window(seconds: i64) -> Value {
    RING.lock()
        .map(|r| r.window(now_unix_micros(), seconds))
        .unwrap_or_else(|_| json!({"samples": [], "count": 0}))
}

/// Sample the radio's status sidecar into [`RING`] forever.
pub async fn run_sampler(config_path: PathBuf, run_dir: PathBuf) {
    let sidecar = run_dir.join(ados_protocol::paths::WFB_STATS_JSON);
    let mut tick = tokio::time::interval(SAMPLE_PERIOD);
    loop {
        tick.tick().await;
        let status = sample_once(&config_path, &sidecar).await;
        if let Ok(mut ring) = RING.lock() {
            ring.push(now_unix_micros(), &status);
        }
    }
}

async fn sample_once(config_path: &Path, sidecar: &Path) -> Value {
    let (config_path, sidecar) = (config_path.to_path_buf(), sidecar.to_path_buf());
    tokio::task::spawn_blocking(move || {
        build_status_from_stats_file_at(&WfbStatusConfig::load(&config_path), &sidecar)
    })
    .await
    .unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live(rssi: f64) -> Value {
        json!({"state": "active", "packets_received": 339, "rssi_dbm": rssi, "snr_db": 20.0,
               "loss_percent": 0.5, "bitrate_kbps": 8000})
    }

    #[test]
    fn history_grows_with_each_live_status_tick() {
        let mut ring = LinkHistory::default();
        let t0 = 1_000_000_000_000;
        for i in 0..5 {
            ring.push(t0 + i * 2_000_000, &live(-50.0 - i as f64));
        }
        let out = ring.window(t0 + 8_000_000, 60);
        assert_eq!(out["count"], json!(5));
        assert_eq!(out["samples"][4]["rssi_dbm"], json!(-54.0));
        assert!(out["samples"][0]["timestamp"].is_string());
    }

    #[test]
    fn a_silent_or_stale_status_adds_nothing_and_old_samples_age_out() {
        let mut ring = LinkHistory::default();
        let t0 = 1_000_000_000_000;
        ring.push(t0, &live(-50.0));
        ring.push(t0 + 1, &json!({"state": "active", "packets_received": 0}));
        ring.push(t0 + 2, &json!({"state": "stale", "packets_received": 10}));
        assert_eq!(ring.window(t0 + 3, 60)["count"], json!(1));
        // Past the window the sample is out of a 60 s read, and past the ring's
        // span it is dropped outright.
        assert_eq!(ring.window(t0 + 61_000_000, 60)["count"], json!(0));
        ring.push(t0 + 301_000_000, &json!({"packets_received": 0}));
        assert!(ring.samples.is_empty());
    }
}
