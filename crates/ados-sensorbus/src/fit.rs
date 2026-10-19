//! Clock relation math: a windowed linear fit of a source clock against the
//! host monotonic clock (offset + skew, sigma = residual RMS), and the
//! NTP-style TIMESYNC estimator built on it.

use std::collections::VecDeque;

/// Least-squares fit of `offset(host) = source - host` as a line in host
/// time, over the pairs of the last `window_ns`.
#[derive(Debug, Clone)]
pub struct ClockFit {
    window_ns: u64,
    max_points: usize,
    pts: VecDeque<(u64, i64)>,
    model: Option<Model>,
}

#[derive(Debug, Clone, Copy)]
struct Model {
    /// Reference host time the line is centred on.
    x0: u64,
    /// Offset at `x0`, ns.
    c: f64,
    /// Slope: ns of offset per ns of host time (skew).
    s: f64,
    /// Residual RMS, ns.
    rms: f64,
}

impl ClockFit {
    /// A fit over the last `window_ns` of pairs (at most `max_points`).
    pub fn new(window_ns: u64, max_points: usize) -> Self {
        ClockFit {
            window_ns,
            max_points: max_points.max(2),
            pts: VecDeque::new(),
            model: None,
        }
    }

    /// The default 60 s window.
    pub fn sixty_seconds() -> Self {
        Self::new(60_000_000_000, 4096)
    }

    /// Add one pair: at host monotonic `host_ns` the source read `source_ns`.
    pub fn push(&mut self, host_ns: u64, source_ns: i64) {
        self.pts.push_back((host_ns, source_ns));
        let newest = host_ns;
        while let Some(&(h, _)) = self.pts.front() {
            if newest.saturating_sub(h) > self.window_ns || self.pts.len() > self.max_points {
                self.pts.pop_front();
            } else {
                break;
            }
        }
        self.refit();
    }

    /// Drop every pair (source reset, e.g. a flight-controller reboot).
    pub fn reset(&mut self) {
        self.pts.clear();
        self.model = None;
    }

    pub fn len(&self) -> usize {
        self.pts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pts.is_empty()
    }

    /// Host time of the newest pair.
    pub fn last_host_ns(&self) -> Option<u64> {
        self.pts.back().map(|p| p.0)
    }

    fn refit(&mut self) {
        let n = self.pts.len();
        if n == 0 {
            self.model = None;
            return;
        }
        let x0 = self.pts.back().unwrap().0;
        let pts = || {
            self.pts
                .iter()
                .map(move |&(h, s)| (h as f64 - x0 as f64, (s as i128 - h as i128) as f64))
        };
        let nf = n as f64;
        let (sx, sy) = pts().fold((0.0, 0.0), |(a, b), (x, y)| (a + x, b + y));
        let (mx, my) = (sx / nf, sy / nf);
        let (sxx, sxy) = pts().fold((0.0, 0.0), |(a, b), (x, y)| {
            (a + (x - mx) * (x - mx), b + (x - mx) * (y - my))
        });
        let s = if n >= 2 && sxx > 0.0 { sxy / sxx } else { 0.0 };
        let c = my - s * mx;
        let rss = pts().fold(0.0, |a, (x, y)| {
            let r = y - (c + s * x);
            a + r * r
        });
        let rms = if n > 2 {
            (rss / (nf - 2.0)).sqrt()
        } else {
            0.0
        };
        self.model = Some(Model { x0, c, s, rms });
    }

    /// Offset `source - host` at host time `host_ns`, ns.
    pub fn offset_ns(&self, host_ns: u64) -> Option<f64> {
        let m = self.model?;
        Some(m.c + m.s * (host_ns as f64 - m.x0 as f64))
    }

    /// Source time at host time `host_ns`.
    pub fn to_source(&self, host_ns: u64) -> Option<i64> {
        self.offset_ns(host_ns)
            .map(|o| (host_ns as i128 + o.round() as i128) as i64)
    }

    /// Host time at source time `source_ns` (inverse of [`Self::to_source`]).
    pub fn to_host(&self, source_ns: i64) -> Option<u64> {
        let m = self.model?;
        // source = host + c + s (host - x0)  =>  host = (source - c + s x0) / (1 + s)
        let host = (source_ns as f64 - m.c + m.s * m.x0 as f64) / (1.0 + m.s);
        (host >= 0.0).then_some(host.round() as u64)
    }

    /// Skew of the source against the host, parts per million.
    pub fn skew_ppm(&self) -> f64 {
        self.model.map_or(0.0, |m| m.s * 1e6)
    }

    /// One-sigma residual, ns; `-1` with fewer than three pairs (unmeasured).
    pub fn sigma_ns(&self) -> i64 {
        match self.model {
            Some(m) if self.pts.len() >= 3 => m.rms.round() as i64,
            _ => -1,
        }
    }
}

/// Default number of recent round trips the min-RTT gate looks back over.
pub const RTT_WINDOW: usize = 32;

/// Two-way time exchange estimator (MAVLink TIMESYNC). Each completed
/// exchange gives `host_tx`, the source's reply time `source_ns`, and
/// `host_rx`; the source time is taken to correspond to the midpoint. Only
/// exchanges whose round trip is within a margin of the minimum over the last
/// [`RTT_WINDOW`] feed the fit, which rejects queueing-delayed replies whose
/// asymmetry would bias the offset.
#[derive(Debug, Clone)]
pub struct TimesyncEstimator {
    rtts: VecDeque<u64>,
    fit: ClockFit,
}

/// Accepted round trips may exceed the window minimum by this much.
const RTT_MARGIN_NS: u64 = 20_000;

impl Default for TimesyncEstimator {
    fn default() -> Self {
        TimesyncEstimator {
            rtts: VecDeque::with_capacity(RTT_WINDOW),
            fit: ClockFit::sixty_seconds(),
        }
    }
}

impl TimesyncEstimator {
    /// Feed one completed exchange. Returns the accepted `(host_mid_ns,
    /// source_ns, rtt_ns)` pair, or `None` when the min-RTT gate rejected it.
    pub fn on_exchange(
        &mut self,
        host_tx_ns: u64,
        source_ns: i64,
        host_rx_ns: u64,
    ) -> Option<(u64, i64, u64)> {
        if host_rx_ns < host_tx_ns {
            return None;
        }
        let rtt = host_rx_ns - host_tx_ns;
        if self.rtts.len() == RTT_WINDOW {
            self.rtts.pop_front();
        }
        self.rtts.push_back(rtt);
        let min = *self.rtts.iter().min().unwrap();
        if rtt > min + RTT_MARGIN_NS.max(min / 10) {
            return None;
        }
        let mid = host_tx_ns + rtt / 2;
        self.fit.push(mid, source_ns);
        Some((mid, source_ns, rtt))
    }

    /// Forget every exchange (source rebooted: its clock jumped).
    pub fn reset(&mut self) {
        self.rtts.clear();
        self.fit.reset();
    }

    pub fn fit(&self) -> &ClockFit {
        &self.fit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// xorshift64*, deterministic test noise.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        /// Exponential-ish delay with the given mean, ns.
        fn delay(&mut self, mean: f64) -> u64 {
            let u = (self.next() >> 11) as f64 / (1u64 << 53) as f64;
            (-(1.0 - u).ln() * mean) as u64
        }
    }

    #[test]
    fn fit_recovers_exact_line() {
        let mut f = ClockFit::sixty_seconds();
        for i in 0..100u64 {
            let h = 1_000_000_000_000 + i * 100_000_000;
            let s = h as i64 + 5_000_000 + (h as f64 * 10e-6) as i64;
            f.push(h, s);
        }
        let h = 1_000_000_000_000 + 99 * 100_000_000;
        let want = 5_000_000.0 + h as f64 * 10e-6;
        assert!((f.offset_ns(h).unwrap() - want).abs() < 2.0);
        assert!((f.skew_ppm() - 10.0).abs() < 1e-3);
        assert!(f.sigma_ns() <= 1);
        let src = f.to_source(h).unwrap();
        assert!((f.to_host(src).unwrap() as i64 - h as i64).abs() <= 1);
    }

    #[test]
    fn fit_window_drops_old_pairs_and_sigma_reports_noise() {
        let mut f = ClockFit::new(10_000_000_000, 4096);
        assert_eq!(f.sigma_ns(), -1);
        // A first regime that must age out of the 10 s window.
        for i in 0..50u64 {
            f.push(i * 100_000_000, (i * 100_000_000) as i64 + 1_000_000);
        }
        let mut rng = Rng(7);
        for i in 0..200u64 {
            let h = 20_000_000_000 + i * 50_000_000;
            let noise = (rng.next() % 2001) as i64 - 1000;
            f.push(h, h as i64 - 3_000_000 + noise);
        }
        assert!(f.len() <= 201);
        let o = f.offset_ns(f.last_host_ns().unwrap()).unwrap();
        assert!((o + 3_000_000.0).abs() < 300.0, "offset {o}");
        let sigma = f.sigma_ns();
        assert!((400..=800).contains(&sigma), "sigma {sigma}");
    }

    /// TIMESYNC: 37.2 ms offset + 40 ppm skew, asymmetric random link delays,
    /// recovered within 2 µs.
    #[test]
    fn timesync_recovers_offset_and_skew_within_two_microseconds() {
        let offset = 37_200_000.0;
        let skew = 40e-6;
        let fc = |host: u64| (host as f64 * (1.0 + skew) + offset) as i64;
        let mut est = TimesyncEstimator::default();
        let mut rng = Rng(0x5eed);
        let mut host = 5_000_000_000u64;
        let mut accepted = 0;
        for _ in 0..600 {
            // 10 Hz exchanges over 60 s, each way 300 µs base + jitter.
            host += 100_000_000;
            let up = 300_000 + rng.delay(20_000.0);
            let down = 300_000 + rng.delay(20_000.0);
            let source = fc(host + up);
            if est.on_exchange(host, source, host + up + down).is_some() {
                accepted += 1;
            }
        }
        assert!(accepted > 50, "accepted {accepted}");
        let t = host;
        let want = fc(t) as f64 - t as f64;
        let got = est.fit().offset_ns(t).unwrap();
        assert!(
            (got - want).abs() < 2_000.0,
            "offset error {} ns",
            got - want
        );
        assert!((est.fit().skew_ppm() - 40.0).abs() < 0.05);
    }
}
