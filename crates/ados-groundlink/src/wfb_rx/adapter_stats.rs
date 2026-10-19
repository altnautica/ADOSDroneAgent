//! Per-adapter receive counters and the diversity block of the `wfb-stats`
//! sidecar.
//!
//! One `wfb_rx` listens on every receiving adapter. Each stats interval it
//! prints one `RX_ANT` line per (frequency, MCS, bandwidth, antenna) and then
//! one `PKT` line for the combined decoder:
//!
//! ```text
//! <ts>\tRX_ANT\t<freq>:<mcs>:<bw>\t<antenna_id hex>\t<count>:<rssi min:avg:max>:<snr min:avg:max>
//! <ts>\tPKT\t<p_all>:<b_all>:<dec_err>:<sess>:<data>:<uniq>:<fec_rec>:<lost>:<bad>:<out>:<b_out>
//! ```
//!
//! The antenna id is `wlan_idx << 8 | chain` (`vendor/wfb-ng/src/rx.cpp`), where
//! `wlan_idx` is the interface's position in the `wfb_rx` argument list, so the
//! receive adapters are recovered in [`AdapterPlan::rx_ifaces`] order. Bits
//! above 32 carry a forwarder address in aggregator mode and are never a local
//! adapter, so those lines are skipped.
//!
//! Per interval the decoder saw `uniq + lost` frames. An adapter's frames are
//! the largest count across its chains (every chain of one radio hears the same
//! frame), and what it did not hear of that total is its estimated loss. It is
//! an estimate: the per-antenna counts include FEC parity frames while the
//! decoder's lost count is data-only. Comparing the best single adapter's loss
//! with the decoder's combined loss over the same window is the diversity gain.

use std::collections::{BTreeMap, VecDeque};

use ados_protocol::radio_adapters::{DiversityReport, RadioAdapterReport, RadioAdaptersBody};
use ados_radio::adapter::AdapterPlan;

/// Stats intervals (one per second) the diversity block compares over.
pub const DIVERSITY_WINDOW: usize = 10;

/// One interval's loss picture, kept for the diversity window.
#[derive(Debug, Clone)]
struct WindowSample {
    expected: u64,
    lost_combined: u64,
    /// Per receive adapter, in `wfb_rx` order.
    lost_single: Vec<u64>,
}

/// One chain's accumulation over the current interval.
#[derive(Debug, Clone, Copy, Default)]
struct ChainAcc {
    count: u64,
    rssi_weighted: i64,
}

/// Folds the receive plane's stats stream into per-adapter reports.
#[derive(Debug, Clone)]
pub struct AdapterStatsTracker {
    reports: Vec<RadioAdapterReport>,
    /// For each receive adapter (`wfb_rx` order), its index in `reports`.
    rx_report: Vec<usize>,
    /// `(wlan_idx, chain)` → this interval's accumulation.
    interval: BTreeMap<(usize, u8), ChainAcc>,
    window: VecDeque<WindowSample>,
}

impl AdapterStatsTracker {
    /// Track every adapter of `plan`. Receive adapters are matched to the
    /// stats stream by their position in [`AdapterPlan::rx_ifaces`].
    pub fn new(plan: &AdapterPlan) -> Self {
        let rx_ifaces = plan.rx_ifaces();
        let single_rx = rx_ifaces.len() == 1;
        let reports: Vec<RadioAdapterReport> = plan
            .adapters()
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let receives = rx_ifaces.contains(&r.adapter.ifname);
                RadioAdapterReport {
                    iface: r.adapter.ifname.clone(),
                    mac: r.adapter.mac.clone(),
                    usb_path: r.adapter.usb_path.clone(),
                    driver: r.adapter.driver.clone(),
                    role: r.role,
                    antenna: r.antenna.clone(),
                    tx_active: plan.is_tx(i),
                    rssi_dbm: Vec::new(),
                    packets: 0,
                    lost: 0,
                    fec_rec: (receives && single_rx).then_some(0),
                    verified: r.adapter.verified,
                }
            })
            .collect();
        let rx_report = rx_ifaces
            .iter()
            .filter_map(|iface| reports.iter().position(|r| &r.iface == iface))
            .collect();
        Self {
            reports,
            rx_report,
            interval: BTreeMap::new(),
            window: VecDeque::with_capacity(DIVERSITY_WINDOW),
        }
    }

    /// Feed one stdout line. `RX_ANT` lines accumulate; a `PKT` line closes
    /// the interval. Every other line is ignored.
    pub fn feed_line(&mut self, line: &str) {
        if let Some(ant) = parse_rx_ant(line) {
            if ant.antenna_id >> 32 != 0 {
                return;
            }
            let wlan_idx = ((ant.antenna_id >> 8) & 0xff) as usize;
            let chain = (ant.antenna_id & 0xff) as u8;
            let acc = self.interval.entry((wlan_idx, chain)).or_default();
            acc.count += ant.count;
            acc.rssi_weighted += i64::from(ant.rssi_avg) * ant.count as i64;
        } else if let Some(pkt) = parse_pkt(line) {
            self.close_interval(pkt);
        }
    }

    fn close_interval(&mut self, pkt: PktCounts) {
        let expected = pkt.uniq + pkt.lost;
        let single_rx = self.rx_report.len() == 1;
        let mut lost_single = Vec::with_capacity(self.rx_report.len());
        for (wlan_idx, &report_idx) in self.rx_report.iter().enumerate() {
            let chains: Vec<(u8, ChainAcc)> = self
                .interval
                .range((wlan_idx, 0)..=(wlan_idx, u8::MAX))
                .map(|(&(_, chain), acc)| (chain, *acc))
                .collect();
            let received = chains.iter().map(|(_, a)| a.count).max().unwrap_or(0);
            let lost = if single_rx {
                pkt.lost
            } else {
                expected - received.min(expected)
            };
            let report = &mut self.reports[report_idx];
            report.packets += received;
            report.lost += lost;
            if single_rx {
                report.fec_rec = Some(report.fec_rec.unwrap_or(0) + pkt.fec_rec);
            }
            report.rssi_dbm = chains
                .iter()
                .filter(|(_, a)| a.count > 0)
                .map(|(_, a)| (a.rssi_weighted as f64 / a.count as f64).round() as i32)
                .collect();
            lost_single.push(lost);
        }
        self.interval.clear();
        if self.window.len() == DIVERSITY_WINDOW {
            self.window.pop_front();
        }
        self.window.push_back(WindowSample {
            expected,
            lost_combined: pkt.lost,
            lost_single,
        });
    }

    /// The body as of the last closed interval. A live tracker is a reporting
    /// receive plane, so `present` is true.
    pub fn body(&self) -> RadioAdaptersBody {
        RadioAdaptersBody {
            present: true,
            adapters: self.reports.clone(),
            diversity: self.diversity(),
        }
    }

    fn diversity(&self) -> DiversityReport {
        let expected: u64 = self.window.iter().map(|s| s.expected).sum();
        let pct = |lost: u64| (lost as f64 * 10_000.0 / expected as f64).round() / 100.0;
        let (combined, best) = if expected == 0 {
            (None, None)
        } else {
            let combined: u64 = self.window.iter().map(|s| s.lost_combined).sum();
            let best = (0..self.rx_report.len())
                .map(|i| {
                    self.window
                        .iter()
                        .map(|s| s.lost_single.get(i).copied().unwrap_or(0))
                        .sum::<u64>()
                })
                .min();
            (Some(pct(combined)), best.map(pct))
        };
        DiversityReport {
            rx_adapters: self.rx_report.len() as u32,
            combined_loss_pct: combined,
            best_single_loss_pct: best,
        }
    }

    /// Write `adapters` and `diversity` onto a sidecar payload object.
    pub fn insert_into(&self, payload: &mut serde_json::Value) {
        insert_body(payload, &self.body());
    }
}

/// Write a `{adapters, diversity}` body onto a sidecar payload object.
pub fn insert_body(payload: &mut serde_json::Value, body: &RadioAdaptersBody) {
    if let Some(obj) = payload.as_object_mut() {
        obj.insert(
            "adapters".to_string(),
            serde_json::to_value(&body.adapters).unwrap_or_else(|_| serde_json::json!([])),
        );
        obj.insert(
            "diversity".to_string(),
            serde_json::to_value(&body.diversity).unwrap_or(serde_json::Value::Null),
        );
    }
}

/// The `RX_ANT` fields the tracker reads.
struct RxAnt {
    antenna_id: u64,
    count: u64,
    rssi_avg: i32,
}

/// Parse an `RX_ANT` line, or `None` for any other line.
fn parse_rx_ant(line: &str) -> Option<RxAnt> {
    let mut cols = line.trim_end().split('\t');
    let _ts = cols.next()?;
    if cols.next()? != "RX_ANT" {
        return None;
    }
    let _freq_mcs_bw = cols.next()?;
    let antenna_id = u64::from_str_radix(cols.next()?, 16).ok()?;
    let mut f = cols.next()?.split(':');
    let count = f.next()?.parse::<u64>().ok()?;
    let _rssi_min = f.next()?;
    let rssi_avg = f.next()?.parse::<i32>().ok()?;
    Some(RxAnt {
        antenna_id,
        count,
        rssi_avg,
    })
}

/// The `PKT` counters the tracker reads.
struct PktCounts {
    uniq: u64,
    fec_rec: u64,
    lost: u64,
}

/// Parse an 11-field `PKT` line, or `None` for any other line.
fn parse_pkt(line: &str) -> Option<PktCounts> {
    let mut cols = line.trim_end().split('\t');
    let _ts = cols.next()?;
    if cols.next()? != "PKT" {
        return None;
    }
    let f: Vec<u64> = cols
        .next()?
        .split(':')
        .map(|v| v.parse::<u64>().ok())
        .collect::<Option<_>>()?;
    if f.len() < 11 {
        return None;
    }
    Some(PktCounts {
        uniq: f[5],
        fec_rec: f[6],
        lost: f[7],
    })
}

#[cfg(test)]
mod tests;
