//! Fixed-capacity sample rings for the trend charts and sparklines.
//!
//! A sample is recorded only when the value is actually present (and finite)
//! in the payload, so a chart never plots a synthesised zero for a missing
//! reading.

use std::collections::VecDeque;

/// A ring of the most recent `cap` present samples.
#[derive(Debug, Clone)]
pub struct Ring {
    cap: usize,
    buf: VecDeque<f64>,
}

impl Ring {
    pub fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            buf: VecDeque::with_capacity(cap.max(1)),
        }
    }

    /// Record `value` when present and finite; drop the oldest past capacity.
    pub fn push(&mut self, value: Option<f64>) {
        let Some(v) = value.filter(|v| v.is_finite()) else {
            return;
        };
        if self.buf.len() == self.cap {
            self.buf.pop_front();
        }
        self.buf.push_back(v);
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn cap(&self) -> usize {
        self.cap
    }

    pub fn last(&self) -> Option<f64> {
        self.buf.back().copied()
    }

    /// Chart points `(x, y)`, the newest sample at `x = cap - 1` so the trace
    /// grows in from the right edge.
    pub fn points(&self) -> Vec<(f64, f64)> {
        let start = self.cap - self.buf.len();
        self.buf
            .iter()
            .enumerate()
            .map(|(i, v)| ((start + i) as f64, *v))
            .collect()
    }

    /// `(min, max)` of the samples, widened so a flat trace still has a span.
    pub fn bounds(&self) -> Option<(f64, f64)> {
        let mut it = self.buf.iter().copied();
        let first = it.next()?;
        let (lo, hi) = it.fold((first, first), |(lo, hi), v| (lo.min(v), hi.max(v)));
        if (hi - lo).abs() < 1e-9 {
            let pad = (hi.abs() * 0.05).max(0.5);
            Some((lo - pad, hi + pad))
        } else {
            let pad = (hi - lo) * 0.1;
            Some((lo - pad, hi + pad))
        }
    }

    /// The samples scaled for a sparkline: `(v * scale)` rounded, negatives
    /// clamped to zero.
    pub fn spark(&self, scale: f64) -> Vec<u64> {
        self.buf
            .iter()
            .map(|v| (v * scale).round().max(0.0) as u64)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_present_finite_values_are_recorded() {
        let mut r = Ring::new(3);
        r.push(None);
        r.push(Some(f64::NAN));
        assert!(r.is_empty());
        r.push(Some(1.0));
        r.push(None);
        r.push(Some(2.0));
        assert_eq!(r.len(), 2);
        assert_eq!(r.last(), Some(2.0));
    }

    #[test]
    fn the_ring_keeps_the_newest_cap_samples() {
        let mut r = Ring::new(3);
        for v in 0..5 {
            r.push(Some(f64::from(v)));
        }
        assert_eq!(r.points(), vec![(0.0, 2.0), (1.0, 3.0), (2.0, 4.0)]);
    }

    #[test]
    fn a_partly_filled_ring_plots_from_the_right_edge() {
        let mut r = Ring::new(4);
        r.push(Some(5.0));
        assert_eq!(r.points(), vec![(3.0, 5.0)]);
        let (lo, hi) = r.bounds().unwrap();
        assert!(lo < 5.0 && hi > 5.0);
    }
}
