//! Typed radio-link readings from `GET /api/wfb`, the 5-minute trend rings,
//! and the counter-delta rule for cumulative counters.
//!
//! The WFB `fec_recovered` / `fec_failed` / loss / bitrate fields already
//! describe the producer's last 1 s interval. Cumulative counters (the ground
//! station's per-adapter packet counts, the receiver's combined counts, relay
//! fragments, LoRa frame counts) are shown as the difference between
//! consecutive polls, never as raw totals.

use std::collections::HashMap;

use serde_json::Value;

use super::ring::Ring;

/// 150 samples at the 2 s poll = 5 minutes.
pub const LINK_HISTORY: usize = 150;

/// The per-interval increase of a cumulative counter. `None` without a
/// previous sample, and `None` when the counter went backwards (the producer
/// restarted), so a reset never reads as a huge jump.
pub fn delta(prev: Option<u64>, cur: u64) -> Option<u64> {
    let prev = prev?;
    (cur >= prev).then(|| cur.saturating_sub(prev))
}

/// The last value and last delta of every named cumulative counter.
#[derive(Debug, Clone, Default)]
pub struct Counters {
    seen: HashMap<String, (u64, Option<u64>)>,
}

impl Counters {
    /// Feed the current value of `name`; returns its delta since the last feed.
    pub fn feed(&mut self, name: &str, cur: Option<u64>) -> Option<u64> {
        let Some(cur) = cur else {
            self.seen.remove(name);
            return None;
        };
        let prev = self.seen.get(name).map(|(v, _)| *v);
        let d = delta(prev, cur);
        self.seen.insert(name.to_string(), (cur, d));
        d
    }

    /// The delta from the last feed of `name`.
    pub fn get(&self, name: &str) -> Option<u64> {
        self.seen.get(name).and_then(|(_, d)| *d)
    }
}

fn f(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64).filter(|x| x.is_finite())
}

fn i(v: &Value, key: &str) -> Option<i64> {
    v.get(key).and_then(Value::as_i64)
}

fn s(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// One `/api/wfb` reading.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Wfb {
    pub state: String,
    pub interface: Option<String>,
    pub channel: Option<i64>,
    pub actual_channel: Option<i64>,
    pub frequency_mhz: Option<i64>,
    pub bandwidth_mhz: Option<i64>,
    pub mcs_index: Option<i64>,
    pub tx_power_dbm: Option<f64>,
    pub tx_power_max_dbm: Option<f64>,
    pub regulatory_domain: Option<String>,
    pub driver: Option<String>,
    pub chipset: Option<String>,
    pub restart_count: Option<i64>,
    pub rssi_dbm: Option<f64>,
    pub snr_db: Option<f64>,
    pub noise_dbm: Option<f64>,
    pub loss_percent: Option<f64>,
    pub bitrate_kbps: Option<f64>,
    pub fec_recovered: Option<i64>,
    pub fec_failed: Option<i64>,
    pub link_diag: Option<String>,
    pub ground: bool,
}

impl Wfb {
    pub fn from_value(v: &Value) -> Self {
        let adapter = v.get("adapter").cloned().unwrap_or(Value::Null);
        // Only a decoded packet makes the radio figures real.
        let rx = i(v, "packets_received").is_some_and(|n| n > 0);
        Self {
            state: s(v, "state").unwrap_or_else(|| "unknown".into()),
            interface: s(v, "interface"),
            channel: i(v, "channel"),
            actual_channel: i(v, "actual_channel"),
            frequency_mhz: i(v, "frequency_mhz").filter(|n| *n > 0),
            bandwidth_mhz: i(v, "bandwidth_mhz").filter(|n| *n > 0),
            mcs_index: i(v, "mcs_index"),
            tx_power_dbm: f(v, "tx_power_dbm"),
            tx_power_max_dbm: f(v, "tx_power_max_dbm"),
            regulatory_domain: s(v, "regulatory_domain"),
            driver: s(&adapter, "driver"),
            chipset: s(v, "adapter_chipset").or_else(|| s(&adapter, "chipset")),
            restart_count: i(v, "restart_count"),
            rssi_dbm: f(v, "rssi_dbm"),
            snr_db: f(v, "snr_db"),
            noise_dbm: f(v, "noise_dbm"),
            loss_percent: if rx { f(v, "loss_percent") } else { None },
            bitrate_kbps: f(v, "bitrate_kbps"),
            fec_recovered: i(v, "fec_recovered"),
            fec_failed: i(v, "fec_failed"),
            link_diag: s(v, "link_diag"),
            ground: v.get("profile").and_then(Value::as_str) == Some("ground_station"),
        }
    }
}

/// The 2 s trend rings.
#[derive(Debug, Clone)]
pub struct LinkHistory {
    pub rssi: Ring,
    pub snr: Ring,
    pub loss: Ring,
    pub bitrate: Ring,
}

impl Default for LinkHistory {
    fn default() -> Self {
        Self {
            rssi: Ring::new(LINK_HISTORY),
            snr: Ring::new(LINK_HISTORY),
            loss: Ring::new(LINK_HISTORY),
            bitrate: Ring::new(LINK_HISTORY),
        }
    }
}

impl LinkHistory {
    pub fn record(&mut self, w: &Wfb) {
        self.rssi.push(w.rssi_dbm);
        self.snr.push(w.snr_db);
        self.loss.push(w.loss_percent);
        self.bitrate.push(w.bitrate_kbps);
    }
}

/// A non-negative JSON integer as `u64`.
pub fn u(v: &Value, key: &str) -> Option<u64> {
    v.get(key).and_then(Value::as_u64)
}

/// Feed the cumulative counters of one poll: the ground station's adapters
/// (`adapters[].packets/lost/fec_rec`), from a `/api/wfb` body.
pub fn feed_adapters(counters: &mut Counters, wfb: &Value) {
    for a in wfb
        .get("adapters")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let iface = a.get("iface").and_then(Value::as_str).unwrap_or("?");
        for key in ["packets", "lost", "fec_rec"] {
            counters.feed(&format!("adapter:{iface}:{key}"), u(a, key));
        }
    }
}

/// Feed the receiver's combined counters.
pub fn feed_combined(counters: &mut Counters, combined: &Value) {
    for key in ["fragments_after_dedup", "fec_repaired"] {
        counters.feed(&format!("combined:{key}"), u(combined, key));
    }
}

/// Feed each relay's fragment count.
pub fn feed_relays(counters: &mut Counters, relays: &Value) {
    for r in relays
        .get("relays")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let mac = r.get("mac").and_then(Value::as_str).unwrap_or("?");
        counters.feed(&format!("relay:{mac}"), u(r, "fragments"));
    }
}

/// Feed the LoRa lane's cumulative frame counters.
pub fn feed_lora(counters: &mut Counters, lora: &Value) {
    let get = |a: &str, b: &str| lora.get(a).and_then(|x| u(x, b));
    counters.feed("lora:tx", get("tx", "frames"));
    counters.feed("lora:refused", get("tx", "refused"));
    counters.feed("lora:rx", get("rx", "frames"));
    counters.feed("lora:crc", get("rx", "crc_errors"));
}

/// A 0–255 batman TQ as a 10-cell bar.
pub fn tq_bar(tq: i64, full: &str, empty: &str) -> String {
    let filled = usize::try_from(tq.clamp(0, 255) * 10 / 255).unwrap_or(0);
    format!("{}{}", full.repeat(filled), empty.repeat(10 - filled))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_counter_decrease_yields_none_not_a_huge_delta() {
        assert_eq!(delta(None, 10), None);
        assert_eq!(delta(Some(10), 15), Some(5));
        assert_eq!(delta(Some(10), 10), Some(0));
        assert_eq!(delta(Some(500), 3), None);
    }

    #[test]
    fn counters_track_deltas_and_forget_missing_values() {
        let mut c = Counters::default();
        assert_eq!(c.feed("x", Some(100)), None);
        assert_eq!(c.feed("x", Some(130)), Some(30));
        assert_eq!(c.get("x"), Some(30));
        assert_eq!(c.feed("x", Some(4)), None, "restart");
        assert_eq!(c.feed("x", Some(9)), Some(5));
        assert_eq!(c.feed("x", None), None);
        assert_eq!(c.feed("x", Some(12)), None, "no baseline after a gap");
    }

    #[test]
    fn link_history_records_only_present_values() {
        let mut h = LinkHistory::default();
        let w = Wfb::from_value(&json!({
            "state": "searching", "rssi_dbm": null, "snr_db": null,
            "packets_received": 0, "loss_percent": 0.0, "bitrate_kbps": 0
        }));
        h.record(&w);
        assert!(h.rssi.is_empty() && h.snr.is_empty() && h.loss.is_empty());
        assert_eq!(h.bitrate.len(), 1);
        let w = Wfb::from_value(&json!({
            "state": "connected", "rssi_dbm": -52.0, "snr_db": 28.0,
            "packets_received": 180, "loss_percent": 2.5, "bitrate_kbps": 2000
        }));
        h.record(&w);
        assert_eq!(h.rssi.last(), Some(-52.0));
        assert_eq!(h.loss.last(), Some(2.5));
    }

    #[test]
    fn wfb_reads_the_chipset_and_drops_unknown_frequency() {
        let w = Wfb::from_value(&json!({
            "state": "connected", "frequency_mhz": 0, "adapter": {"driver": "", "chipset": ""},
            "adapter_chipset": "RTL8812AU", "mcs_index": null
        }));
        assert_eq!(w.frequency_mhz, None);
        assert_eq!(w.chipset.as_deref(), Some("RTL8812AU"));
        assert_eq!(w.driver, None);
        assert_eq!(w.mcs_index, None);
    }

    #[test]
    fn tq_bar_scales_0_to_255() {
        assert_eq!(tq_bar(255, "#", "."), "##########");
        assert_eq!(tq_bar(0, "#", "."), "..........");
        assert_eq!(tq_bar(128, "#", "."), "#####.....");
    }
}
