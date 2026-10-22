//! Lower-envelope clock fit for pairs stamped on arrival.
//!
//! A pair stamped when it reaches the host (the simulator time carried in a
//! video frame's SEI) includes the encode, transport and decode latency,
//! which only ever makes `host - source` larger and is jittery. The lower
//! envelope of `host - source` (the upper envelope of the offset
//! `source - host`) is therefore the true relation shifted by the
//! pipeline's minimum latency, which one-way pairs cannot measure; the
//! caller adds a known floor or carries its bound in sigma. The fit is the
//! line on or above every
//! pair of the window that minimises the summed lateness of all of them (the
//! linear program of one-way delay skew estimation); its optimum is the edge
//! of the upper convex hull that spans the mean host time. Late pairs never
//! move it.
//!
//! Only the least-late pair of each [`ENVELOPE_SLOT_NS`] host slot is kept:
//! a later one in the same slot cannot touch the envelope, and the window
//! then holds the same span whatever the pair rate (cameras × frame rate).
//!
//! Sigma is the RMS of the envelope points' residuals: per whole
//! [`ENVELOPE_BIN_NS`] host bin, the smallest lateness against the line.
//!
//! The latency floor is touched only now and then (with ~100 ms of jitter,
//! a pair within 1 ms of it arrives every few seconds), so the slope needs a
//! long baseline before the line extrapolated to now is good to well under
//! a millisecond: an envelope locks only once its pairs span
//! [`ENVELOPE_LOCK_SPAN_NS`].

use std::collections::VecDeque;

/// Host-time slot whose least-late pair stands for every pair in it.
pub const ENVELOPE_SLOT_NS: u64 = 50_000_000;
/// Host-time bin each envelope point (for sigma) is the minimum of.
pub const ENVELOPE_BIN_NS: u64 = 2_000_000_000;
/// Completed bins sigma needs; fewer and sigma is unmeasured (`-1`).
pub const ENVELOPE_SIGMA_BINS: u32 = 3;
/// Host-time span of pairs an envelope needs before its source can lock.
pub const ENVELOPE_LOCK_SPAN_NS: u64 = 40_000_000_000;
/// Below this span the slope is unknowable under the jitter (two pairs
/// 50 ms apart can differ by 200 ms of latency); the line is flat through
/// the least-late pair until then.
pub const ENVELOPE_SLOPE_SPAN_NS: u64 = 2_000_000_000;

/// Lower-envelope fit of `offset(host) = source - host` for pairs stamped on
/// arrival, over the pairs of the last window.
#[derive(Debug, Clone)]
pub struct EnvelopeFit {
    window_ns: u64,
    /// `(host_ns, source - host)`, ascending host time, one per slot.
    pts: VecDeque<(u64, i128)>,
    /// Upper hull scratch, reused across refits.
    hull: Vec<(u64, i128)>,
    model: Option<Line>,
}

/// `source - host = y0 + s (host - x0)`.
#[derive(Debug, Clone, Copy)]
struct Line {
    x0: u64,
    y0: i128,
    s: f64,
    sigma_ns: i64,
}

impl Line {
    fn rel(&self, host_ns: u64) -> f64 {
        self.s * (host_ns as i128 - self.x0 as i128) as f64
    }
}

impl EnvelopeFit {
    /// A fit over the last `window_ns` of pairs.
    pub fn new(window_ns: u64) -> Self {
        EnvelopeFit {
            window_ns,
            pts: VecDeque::new(),
            hull: Vec::new(),
            model: None,
        }
    }

    /// The default 60 s window.
    pub fn sixty_seconds() -> Self {
        Self::new(60_000_000_000)
    }

    /// Add one pair: the source read `source_ns` no later than host `host_ns`.
    pub fn push(&mut self, host_ns: u64, source_ns: i64) {
        let y = source_ns as i128 - host_ns as i128;
        // Pairs arrive nearly in order (several streams interleave).
        let mut at = self.pts.len();
        while at > 0 && self.pts[at - 1].0 > host_ns {
            at -= 1;
        }
        // A pair already in this slot sits right before or at `at`.
        let slot = host_ns / ENVELOPE_SLOT_NS;
        let same = [at.checked_sub(1), Some(at)]
            .into_iter()
            .flatten()
            .find(|&i| {
                self.pts
                    .get(i)
                    .is_some_and(|p| p.0 / ENVELOPE_SLOT_NS == slot)
            });
        if let Some(i) = same {
            if self.pts[i].1 >= y {
                return;
            }
            self.pts.remove(i);
            if i < at {
                at -= 1;
            }
        }
        self.pts.insert(at, (host_ns, y));
        let newest = self.pts.back().map_or(host_ns, |p| p.0);
        while let Some(&(h, _)) = self.pts.front() {
            if newest.saturating_sub(h) > self.window_ns {
                self.pts.pop_front();
            } else {
                break;
            }
        }
        self.refit();
    }

    /// Drop every pair.
    pub fn reset(&mut self) {
        self.pts.clear();
        self.model = None;
    }

    /// Pairs currently kept (at most one per host slot).
    pub fn len(&self) -> usize {
        self.pts.len()
    }

    /// Whether no pairs are kept.
    pub fn is_empty(&self) -> bool {
        self.pts.is_empty()
    }

    /// Host time between the oldest and the newest pair in the window.
    pub fn span_ns(&self) -> u64 {
        match (self.pts.front(), self.pts.back()) {
            (Some(a), Some(b)) => b.0 - a.0,
            _ => 0,
        }
    }

    fn refit(&mut self) {
        let (Some(&(oldest, _)), Some(&(newest, _))) = (self.pts.front(), self.pts.back()) else {
            self.model = None;
            return;
        };
        if newest - oldest < ENVELOPE_SLOPE_SPAN_NS {
            let (x0, y0) = self
                .pts
                .iter()
                .copied()
                .max_by_key(|p| p.1)
                .unwrap_or((oldest, 0));
            let mut line = Line {
                x0,
                y0,
                s: 0.0,
                sigma_ns: -1,
            };
            line.sigma_ns = self.sigma(&line);
            self.model = Some(line);
            return;
        }
        // Upper hull of (host, source - host), left to right.
        self.hull.clear();
        for &(x, y) in &self.pts {
            if let Some(&(lx, ly)) = self.hull.last() {
                if lx == x {
                    if y <= ly {
                        continue;
                    }
                    self.hull.pop();
                }
            }
            while let [.., (ax, ay), (bx, by)] = self.hull[..] {
                // Drop b unless c turns clockwise from a→b (b stays above a–c).
                let cross = (bx - ax) as i128 * (y - ay) - (by - ay) * (x - ax) as i128;
                if cross >= 0 {
                    self.hull.pop();
                } else {
                    break;
                }
            }
            self.hull.push((x, y));
        }
        let Some(&first) = self.hull.first() else {
            self.model = None;
            return;
        };
        let n = self.pts.len() as u128;
        let sum: u128 = self.pts.iter().map(|&(x, _)| (x - first.0) as u128).sum();
        let mean_x = first.0 + (sum / n) as u64;
        let (a, b) = self
            .hull
            .windows(2)
            .map(|w| (w[0], w[1]))
            .find(|&(_, b)| b.0 >= mean_x)
            .unwrap_or((first, first));
        let s = if b.0 > a.0 {
            (b.1 - a.1) as f64 / (b.0 - a.0) as f64
        } else {
            0.0
        };
        let mut line = Line {
            x0: a.0,
            y0: a.1,
            s,
            sigma_ns: -1,
        };
        line.sigma_ns = self.sigma(&line);
        self.model = Some(line);
    }

    /// RMS of each bin's smallest lateness, over the bins the window holds
    /// whole: the oldest (cut by the window or the start) and the newest
    /// (still filling) are left out.
    fn sigma(&self, line: &Line) -> i64 {
        let (Some(&(oldest, _)), Some(&(newest, _))) = (self.pts.front(), self.pts.back()) else {
            return -1;
        };
        let (first_bin, open_bin) = (oldest / ENVELOPE_BIN_NS, newest / ENVELOPE_BIN_NS);
        let mut bin = u64::MAX;
        let mut min = f64::INFINITY;
        let (mut sum_sq, mut bins) = (0.0f64, 0u32);
        for &(x, y) in &self.pts {
            let b = x / ENVELOPE_BIN_NS;
            if b == first_bin {
                continue;
            }
            if b == open_bin {
                break;
            }
            if b != bin {
                if min.is_finite() {
                    sum_sq += min * min;
                    bins += 1;
                }
                bin = b;
                min = f64::INFINITY;
            }
            let late = ((line.y0 - y) as f64 + line.rel(x)).max(0.0);
            min = min.min(late);
        }
        if min.is_finite() {
            sum_sq += min * min;
            bins += 1;
        }
        if bins < ENVELOPE_SIGMA_BINS {
            return -1;
        }
        (sum_sq / f64::from(bins)).sqrt().round() as i64
    }

    /// Offset `source - host` at host time `host_ns`, rounded to whole ns.
    pub fn offset_i64(&self, host_ns: u64) -> Option<i64> {
        let m = self.model?;
        Some((m.y0 + m.rel(host_ns).round() as i128) as i64)
    }

    /// Source time at host time `host_ns`.
    pub fn to_source(&self, host_ns: u64) -> Option<i64> {
        self.offset_i64(host_ns)
            .map(|o| (host_ns as i128 + o as i128) as i64)
    }

    /// Skew of the source against the host, parts per million.
    pub fn skew_ppm(&self) -> f64 {
        self.model.map_or(0.0, |m| m.s * 1e6)
    }

    /// One-sigma envelope residual, ns; `-1` until [`ENVELOPE_SIGMA_BINS`]
    /// bins completed.
    pub fn sigma_ns(&self) -> i64 {
        self.model.map_or(-1, |m| m.sigma_ns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    #[test]
    fn a_noiseless_line_is_recovered_exactly() {
        let mut f = EnvelopeFit::sixty_seconds();
        for i in 0..100u64 {
            let h = 1_000 * MS + i * 100 * MS;
            // 40 ppm skew, 37 ms offset.
            f.push(h, (h as f64 * (1.0 + 40e-6)) as i64 - 37 * MS as i64);
        }
        let h = 1_000 * MS + 99 * 100 * MS;
        let want = (h as f64 * (1.0 + 40e-6)) as i64 - 37 * MS as i64;
        assert!(f.to_source(h).unwrap().abs_diff(want) <= 2);
        assert!((f.skew_ppm() - 40.0).abs() < 0.01);
        assert!(f.sigma_ns() <= 2, "sigma {}", f.sigma_ns());
    }

    #[test]
    fn late_pairs_do_not_move_the_line() {
        let mut f = EnvelopeFit::sixty_seconds();
        for i in 0..50u64 {
            let h = i * 100 * MS;
            f.push(h, h as i64);
        }
        let before = f.offset_i64(49 * 100 * MS);
        // Arrived 200 ms late (source behind), and out of order; the second
        // shares the 4.90 s pair's slot and is dropped for it.
        f.push(4_950 * MS, (4_750 * MS) as i64);
        f.push(4_920 * MS, (4_720 * MS) as i64);
        assert_eq!(f.offset_i64(49 * 100 * MS), before);
        assert_eq!(f.len(), 51);
    }

    #[test]
    fn each_slot_keeps_its_least_late_pair() {
        let mut f = EnvelopeFit::sixty_seconds();
        // Four streams at 30 Hz for 70 s: 120 pairs a second.
        for k in 0..70 * 30u64 {
            for cam in 0..4u64 {
                let h = k * 33_333_333 + cam * 7 * MS;
                // Latency 0, 3, 6 or 9 ms by stream.
                f.push(h, h as i64 - (cam * 3 * MS) as i64);
            }
        }
        assert!(f.len() <= (60_000 / 50) as usize + 1, "{} pairs", f.len());
        assert!(f.span_ns() >= 59_900 * MS, "span {}", f.span_ns());
        // The latency-0 stream's pairs are the ones kept.
        assert_eq!(f.offset_i64(69 * 1_000 * MS), Some(0));
        // A less-late pair replaces the slot's pair.
        let h = 70_000 * MS + 10 * MS;
        f.push(h, h as i64 - 20 * MS as i64);
        f.push(h + 5 * MS, (h + 5 * MS) as i64);
        assert_eq!(f.pts.back(), Some(&(h + 5 * MS, 0)));
    }

    #[test]
    fn a_young_fit_is_flat_through_the_least_late_pair() {
        let mut f = EnvelopeFit::sixty_seconds();
        // 50 ms apart, the first 200 ms later than the second.
        f.push(1_000 * MS, (800 * MS) as i64);
        f.push(1_050 * MS, (1_050 * MS) as i64);
        assert_eq!(f.skew_ppm(), 0.0);
        assert_eq!(f.offset_i64(3_000 * MS), Some(0));
    }

    #[test]
    fn sigma_waits_for_three_whole_bins() {
        let mut f = EnvelopeFit::sixty_seconds();
        let mut h = 0;
        while h < 4 * ENVELOPE_BIN_NS {
            f.push(h, h as i64);
            h += 100 * MS;
        }
        assert_eq!(
            f.sigma_ns(),
            -1,
            "the oldest bin is left out, the fourth is open"
        );
        f.push(h, h as i64);
        assert_eq!(f.sigma_ns(), 0);
    }
}
