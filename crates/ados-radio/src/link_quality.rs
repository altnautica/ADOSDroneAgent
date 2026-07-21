//! WFB link-quality monitor — parses the `wfb_rx -l 1000` stats stream.
//!
//! Mirrors `services/wfb/link_quality.py`. wfb-ng (v25+) emits, per stats
//! interval, one or more `RX_ANT` lines followed by one `PKT` line on the
//! receiver's stdout:
//!
//! ```text
//! <ts_ms>\tRX_ANT\t<freq>:<mcs>:<bw>\t<ant_hex>\t<cnt>:<rmin>:<ravg>:<rmax>:<smin>:<savg>:<smax>
//! <ts_ms>\tPKT\t<p_all>:<b_all>:<dec_err>:<sess>:<data>:<uniq>:<fec_rec>:<lost>:<bad>:<out>:<b_out>
//! ```
//!
//! We accumulate the latest RX_ANT and emit a unified [`LinkStats`] on every
//! line so RSSI/SNR freshness never depends on the matching PKT also parsing.
//! Fields drive the `wfb-stats.json` link-quality block and the reactive hop
//! trigger. Parsing is tab/colon split (no regex dependency).

use serde::Serialize;

/// Default stats interval (the `-l 1000` ms → 1 s) used for the bitrate divisor.
const STATS_INTERVAL_S: f64 = 1.0;

/// A one-glance verdict on WHY the RX link is or is not carrying data, derived
/// from the `wfb_rx` PKT counters. `all` (packets captured off-air, pre-decrypt)
/// and `dec_err` (decrypt/session failures) separate the three failure modes that
/// otherwise all read as "no data": a deaf radio (`all==0`, no RF arriving), a
/// wrong-key / wrong-link_id link (`all>0` but `dec_err>0`, RF arriving but not
/// decodable), and a healthy link (`data>0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkDiag {
    /// No stats line observed yet (radio bringing up).
    Searching,
    /// `wfb_rx` alive but 0 packets captured off-air: no RF arriving (TX power /
    /// antenna / channel / the far end not transmitting).
    Deaf,
    /// RF arriving (`all>0`) but 0 decoded and decrypt errors present: keys don't
    /// match, or the link_id / channel_id differs between the two ends.
    MisKeyed,
    /// RF arriving but 0 decoded and corrupt frames present: interference / a
    /// too-weak-to-decode signal.
    Jammed,
    /// Decoded DATA packets are flowing: the link is up.
    Healthy,
}

/// A point-in-time link-quality snapshot (the `wfb-stats.json` link block).
#[derive(Debug, Clone, Serialize)]
pub struct LinkStats {
    pub rssi_dbm: f64,
    pub rssi_min: f64,
    pub rssi_max: f64,
    pub noise_dbm: f64,
    pub snr_db: f64,
    /// Decoded DATA packets this interval — the true "link is up" counter.
    pub packets_received: i64,
    /// All packets captured off-air this interval (pre-decrypt). `0` = deaf radio.
    pub packets_all: i64,
    /// Decrypt / session failures (wrong key or wrong link_id / channel_id).
    pub decrypt_errors: i64,
    /// Corrupt / undecodable frames (interference or a marginal signal).
    pub packets_bad: i64,
    /// Valid session-key packets seen (a peer with matching crypto is present).
    pub session_packets: i64,
    pub packets_lost: i64,
    pub fec_recovered: i64,
    pub fec_failed: i64,
    pub bitrate_kbps: i64,
    pub loss_percent: f64,
    /// One-glance verdict derived from the counters above (deaf / mis_keyed /
    /// jammed / healthy / searching).
    pub link_diag: LinkDiag,
    pub timestamp: String,
}

impl Default for LinkStats {
    fn default() -> Self {
        Self {
            rssi_dbm: -100.0,
            rssi_min: -100.0,
            rssi_max: -100.0,
            noise_dbm: -95.0,
            snr_db: 0.0,
            packets_received: 0,
            packets_all: 0,
            decrypt_errors: 0,
            packets_bad: 0,
            session_packets: 0,
            packets_lost: 0,
            fec_recovered: 0,
            fec_failed: 0,
            bitrate_kbps: 0,
            loss_percent: 0.0,
            link_diag: LinkDiag::Searching,
            timestamp: String::new(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct RxAnt {
    rssi_min: i64,
    rssi_avg: i64,
    rssi_max: i64,
    snr_avg: i64,
}

#[derive(Debug, Clone, Copy, Default)]
struct Pkt {
    all: i64,
    dec_err: i64,
    session: i64,
    data: i64,
    uniq: i64,
    fec_recovered: i64,
    lost: i64,
    bad: i64,
    b_outgoing: i64,
}

/// Parse the colon-delimited tail of an `RX_ANT` line:
/// `<cnt>:<rmin>:<ravg>:<rmax>:<smin>:<savg>:<smax>[:extra...]`.
fn parse_rx_ant(line: &str) -> Option<RxAnt> {
    let cols: Vec<&str> = line.trim_end().split('\t').collect();
    if cols.len() < 5 || cols[1] != "RX_ANT" {
        return None;
    }
    let f: Vec<&str> = cols[4].split(':').collect();
    if f.len() < 7 {
        return None;
    }
    Some(RxAnt {
        rssi_min: f[1].parse().ok()?,
        rssi_avg: f[2].parse().ok()?,
        rssi_max: f[3].parse().ok()?,
        snr_avg: f[5].parse().ok()?,
    })
}

/// Parse the colon-delimited tail of a `PKT` line:
/// `<p_all>:<b_all>:<dec_err>:<sess>:<data>:<uniq>:<fec_rec>:<lost>:<bad>:<out>:<b_out>`.
fn parse_pkt(line: &str) -> Option<Pkt> {
    let cols: Vec<&str> = line.trim_end().split('\t').collect();
    if cols.len() < 3 || cols[1] != "PKT" {
        return None;
    }
    let f: Vec<&str> = cols[2].split(':').collect();
    if f.len() < 11 {
        return None;
    }
    Some(Pkt {
        all: f[0].parse().ok()?,
        dec_err: f[2].parse().ok()?,
        session: f[3].parse().ok()?,
        data: f[4].parse().ok()?,
        uniq: f[5].parse().ok()?,
        fec_recovered: f[6].parse().ok()?,
        lost: f[7].parse().ok()?,
        bad: f[8].parse().ok()?,
        b_outgoing: f[10].parse().ok()?,
    })
}

/// Derive the one-glance link verdict from the PKT counters. `seen` is false until
/// the first PKT line (radio still bringing up → searching). Order matters: a
/// healthy link (decoded data) wins regardless of stray errors; otherwise a deaf
/// radio (nothing off-air) is distinguished from a peer-present-but-undecodable
/// link by whether decrypt errors (wrong key/link_id) or bad frames (interference)
/// dominate.
fn link_diag_of(seen: bool, pkt: &Pkt) -> LinkDiag {
    if !seen {
        return LinkDiag::Searching;
    }
    if pkt.data > 0 {
        return LinkDiag::Healthy;
    }
    if pkt.all == 0 {
        return LinkDiag::Deaf;
    }
    if pkt.dec_err > 0 {
        return LinkDiag::MisKeyed;
    }
    if pkt.bad > 0 {
        return LinkDiag::Jammed;
    }
    // Hearing frames off-air but none classified yet: still acquiring.
    LinkDiag::Searching
}

/// Stateful aggregator: feed it `wfb_rx` stdout lines, read [`current`].
#[derive(Debug, Default)]
pub struct LinkQualityMonitor {
    last_rx: Option<RxAnt>,
    last_pkt: Option<Pkt>,
    latest: LinkStats,
}

impl LinkQualityMonitor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one stdout line. Updates and returns the latest snapshot when the
    /// line is an RX_ANT or PKT; `None` for any other line.
    pub fn feed_line(&mut self, line: &str, now_iso: &str) -> Option<LinkStats> {
        if let Some(rx) = parse_rx_ant(line) {
            self.last_rx = Some(rx);
            return Some(self.build(now_iso));
        }
        if let Some(pkt) = parse_pkt(line) {
            self.last_pkt = Some(pkt);
            return Some(self.build(now_iso));
        }
        None
    }

    /// The most recent snapshot (defaults until the first stats line).
    pub fn current(&self) -> &LinkStats {
        &self.latest
    }

    fn build(&mut self, now_iso: &str) -> LinkStats {
        let (rssi_avg, rssi_min, rssi_max, snr, noise) = match self.last_rx {
            Some(rx) => {
                let snr = rx.snr_avg as f64;
                (
                    rx.rssi_avg as f64,
                    rx.rssi_min as f64,
                    rx.rssi_max as f64,
                    snr,
                    rx.rssi_avg as f64 - snr, // RTL adapters don't publish noise.
                )
            }
            None => (-100.0, -100.0, -100.0, 0.0, -95.0),
        };
        let pkt = self.last_pkt.unwrap_or_default();
        let _ = pkt.uniq; // parsed for completeness; not surfaced.
                          // bitrate = bytes-out-this-interval × 8 / interval (PKT counters reset each interval).
        let bitrate_kbps = (pkt.b_outgoing as f64 * 8.0 / STATS_INTERVAL_S / 1000.0) as i64;
        let denom = pkt.data + pkt.lost;
        let loss_pct = if denom > 0 {
            pkt.lost as f64 / denom as f64 * 100.0
        } else {
            0.0
        };
        let link_diag = link_diag_of(self.last_pkt.is_some(), &pkt);
        let stats = LinkStats {
            rssi_dbm: rssi_avg,
            rssi_min,
            rssi_max,
            noise_dbm: noise,
            snr_db: snr,
            packets_received: pkt.data,
            packets_all: pkt.all,
            decrypt_errors: pkt.dec_err,
            packets_bad: pkt.bad,
            session_packets: pkt.session,
            packets_lost: pkt.lost,
            fec_recovered: pkt.fec_recovered,
            fec_failed: pkt.lost, // upstream "lost" = beyond-FEC failures
            bitrate_kbps,
            loss_percent: (loss_pct * 100.0).round() / 100.0,
            link_diag,
            timestamp: now_iso.to_string(),
        };
        self.latest = stats.clone();
        stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TS: &str = "2026-05-29T00:00:00+00:00";

    #[test]
    fn rx_ant_updates_rssi_snr_noise() {
        let mut m = LinkQualityMonitor::new();
        // ts  RX_ANT  freq:mcs:bw  ant  cnt:rmin:ravg:rmax:smin:savg:smax
        let line = "12345\tRX_ANT\t5745:1:20\t0\t100:-70:-60:-50:8:12:16";
        let s = m.feed_line(line, TS).expect("rx_ant parses");
        assert_eq!(s.rssi_dbm, -60.0);
        assert_eq!(s.rssi_min, -70.0);
        assert_eq!(s.rssi_max, -50.0);
        assert_eq!(s.snr_db, 12.0);
        assert_eq!(s.noise_dbm, -72.0); // rssi_avg - snr = -60 - 12
    }

    #[test]
    fn pkt_updates_packets_loss_bitrate() {
        let mut m = LinkQualityMonitor::new();
        // PKT  p_all:b_all:dec_err:sess:data:uniq:fec_rec:lost:bad:out:b_out
        let line = "12345\tPKT\t200:300000:0:1:180:170:5:20:0:160:250000";
        let s = m.feed_line(line, TS).expect("pkt parses");
        assert_eq!(s.packets_received, 180);
        assert_eq!(s.packets_lost, 20);
        assert_eq!(s.fec_recovered, 5);
        assert_eq!(s.fec_failed, 20);
        // bitrate = 250000 * 8 / 1 / 1000 = 2000 kbps
        assert_eq!(s.bitrate_kbps, 2000);
        // loss = 20 / (180+20) * 100 = 10.0
        assert_eq!(s.loss_percent, 10.0);
        // The formerly-discarded diagnostic counters are now surfaced.
        assert_eq!(s.packets_all, 200);
        assert_eq!(s.decrypt_errors, 0);
        assert_eq!(s.packets_bad, 0);
        assert_eq!(s.session_packets, 1);
        // Decoded data flowing → healthy.
        assert_eq!(s.link_diag, LinkDiag::Healthy);
    }

    #[test]
    fn link_diag_distinguishes_failure_modes() {
        // PKT layout: p_all:b_all:dec_err:sess:data:uniq:fec_rec:lost:bad:out:b_out
        // Deaf: nothing captured off-air (all==0) — no RF arriving.
        let mut m = LinkQualityMonitor::new();
        let s = m.feed_line("1\tPKT\t0:0:0:0:0:0:0:0:0:0:0", TS).unwrap();
        assert_eq!(s.link_diag, LinkDiag::Deaf);
        assert_eq!(s.packets_all, 0);

        // Mis-keyed: RF arriving (all>0) but 0 decoded + decrypt errors present.
        let mut m = LinkQualityMonitor::new();
        let s = m
            .feed_line("1\tPKT\t120:80000:120:0:0:0:0:0:0:0:0", TS)
            .unwrap();
        assert_eq!(s.link_diag, LinkDiag::MisKeyed);
        assert_eq!(s.decrypt_errors, 120);

        // Jammed: RF arriving, 0 decoded, no decrypt errors, corrupt frames.
        let mut m = LinkQualityMonitor::new();
        let s = m
            .feed_line("1\tPKT\t90:60000:0:0:0:0:0:0:90:0:0", TS)
            .unwrap();
        assert_eq!(s.link_diag, LinkDiag::Jammed);
        assert_eq!(s.packets_bad, 90);

        // Healthy: decoded data wins even with a few stray errors.
        let mut m = LinkQualityMonitor::new();
        let s = m
            .feed_line("1\tPKT\t200:300000:2:1:180:180:0:0:1:160:250000", TS)
            .unwrap();
        assert_eq!(s.link_diag, LinkDiag::Healthy);
    }

    #[test]
    fn link_diag_searching_before_any_pkt_line() {
        // Default snapshot (no line yet) and an RX_ANT-only line are still searching.
        let m = LinkQualityMonitor::new();
        assert_eq!(m.current().link_diag, LinkDiag::Searching);
        let mut m = LinkQualityMonitor::new();
        let s = m
            .feed_line("1\tRX_ANT\t5745:1:20\t0\t10:-65:-55:-45:9:14:18", TS)
            .unwrap();
        assert_eq!(s.link_diag, LinkDiag::Searching);
    }

    #[test]
    fn rx_ant_then_pkt_combine() {
        let mut m = LinkQualityMonitor::new();
        m.feed_line("1\tRX_ANT\t5745:1:20\t0\t10:-65:-55:-45:9:14:18", TS);
        let s = m
            .feed_line("1\tPKT\t10:1000:0:1:90:90:0:10:0:80:100000", TS)
            .unwrap();
        // RX_ANT carried forward into the PKT emit.
        assert_eq!(s.rssi_dbm, -55.0);
        assert_eq!(s.snr_db, 14.0);
        assert_eq!(s.packets_received, 90);
        assert_eq!(s.bitrate_kbps, 800); // 100000*8/1000
    }

    #[test]
    fn trailing_extra_columns_tolerated() {
        let mut m = LinkQualityMonitor::new();
        // Some builds append extra counters after smax — must still parse.
        let line = "1\tRX_ANT\t5745:1:20\t0\t10:-65:-55:-45:9:14:18:99:99";
        assert!(m.feed_line(line, TS).is_some());
    }

    #[test]
    fn non_stats_line_returns_none() {
        let mut m = LinkQualityMonitor::new();
        assert!(m.feed_line("some random wfb_rx log line", TS).is_none());
        assert!(m.feed_line("", TS).is_none());
    }

    #[test]
    fn zero_denominator_is_zero_loss() {
        let mut m = LinkQualityMonitor::new();
        let s = m.feed_line("1\tPKT\t0:0:0:0:0:0:0:0:0:0:0", TS).unwrap();
        assert_eq!(s.loss_percent, 0.0);
        assert_eq!(s.packets_received, 0);
    }

    #[test]
    fn current_defaults_before_any_line() {
        let m = LinkQualityMonitor::new();
        assert_eq!(m.current().rssi_dbm, -100.0);
        assert_eq!(m.current().packets_received, 0);
    }
}
