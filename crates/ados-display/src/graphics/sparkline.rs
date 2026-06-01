//! Tiny trend-line sparkline for system metrics.
//!
//! A 60-sample polyline inside a small box. A number alone shows a value; the
//! sparkline next to it shows the trend, so an operator reads "settling" vs
//! "climbing toward a limit" at a glance. Missing samples (`None`) break the
//! line into segments — a clear "no data here" signal rather than a misleading
//! dip to zero.
//!
//! The line auto-scales to the sample range unless `y_min` / `y_max` are pinned
//! (CPU pins to 0..100 so a flat 5 % reads small, not full-box; temperature
//! auto-scales so a 1 degree climb still registers). The geometry matches the
//! prior renderer: samples map across the box width, the value maps up from the
//! bottom edge, and only segments whose both endpoints exist are stroked.

use embedded_graphics::pixelcolor::Rgb888;

use super::primitives::{line, Canvas};

/// How many samples a metric history holds: 60 seconds at the 1 Hz render
/// cadence.
pub const HISTORY_LEN: usize = 60;

/// Draw `samples` as a 1 px polyline inside `(x, y, w, h)`.
///
/// `None` samples create a gap. When `y_min` / `y_max` are `None` the line
/// auto-scales to the present (non-`None`) range; a flat buffer is given a unit
/// span to avoid a divide-by-zero. Fewer than two samples, or an all-`None`
/// buffer, draws nothing.
#[allow(clippy::too_many_arguments)]
pub fn draw_sparkline(
    canvas: &mut Canvas,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    samples: &[Option<f64>],
    color: Rgb888,
    y_min: Option<f64>,
    y_max: Option<f64>,
) {
    let n = samples.len();
    if n < 2 || w == 0 || h == 0 {
        return;
    }
    // Auto-scale across the real samples unless pinned.
    let real: Vec<f64> = samples.iter().filter_map(|s| *s).collect();
    if real.is_empty() {
        return;
    }
    let lo = y_min.unwrap_or_else(|| real.iter().cloned().fold(f64::INFINITY, f64::min));
    let mut hi = y_max.unwrap_or_else(|| real.iter().cloned().fold(f64::NEG_INFINITY, f64::max));
    if hi <= lo {
        hi = lo + 1.0;
    }

    let w_i = w as i32;
    let h_i = h as i32;
    let span = (n - 1) as f64;

    let x_for = |i: usize| -> i32 { x + ((i as f64) * ((w_i - 1) as f64) / span).round() as i32 };
    let y_for = |value: f64| -> i32 {
        let clamped = value.clamp(lo, hi);
        let frac = (clamped - lo) / (hi - lo);
        // Origin is top-left, so invert: a higher value sits nearer the top.
        y + h_i - 1 - (frac * ((h_i - 1) as f64)).round() as i32
    };

    for i in 0..n - 1 {
        let (s0, s1) = (samples[i], samples[i + 1]);
        if let (Some(v0), Some(v1)) = (s0, s1) {
            line(canvas, x_for(i), y_for(v0), x_for(i + 1), y_for(v1), color);
        }
    }
}

#[cfg(test)]
mod tests {
    use embedded_graphics::prelude::RgbColor;

    use super::*;

    fn count_lit(c: &Canvas, color: Rgb888) -> u32 {
        let mut n = 0;
        for y in 0..c.height() as i32 {
            for x in 0..c.width() as i32 {
                if c.pixel(x, y) == color {
                    n += 1;
                }
            }
        }
        n
    }

    #[test]
    fn fewer_than_two_samples_draws_nothing() {
        let mut c = Canvas::new(30, 14, Rgb888::BLACK);
        draw_sparkline(
            &mut c,
            0,
            0,
            30,
            14,
            &[Some(1.0)],
            Rgb888::WHITE,
            None,
            None,
        );
        assert_eq!(count_lit(&c, Rgb888::WHITE), 0);
    }

    #[test]
    fn all_none_draws_nothing() {
        let mut c = Canvas::new(30, 14, Rgb888::BLACK);
        let samples = vec![None, None, None];
        draw_sparkline(&mut c, 0, 0, 30, 14, &samples, Rgb888::WHITE, None, None);
        assert_eq!(count_lit(&c, Rgb888::WHITE), 0);
    }

    #[test]
    fn rising_ramp_inks_pixels() {
        let mut c = Canvas::new(30, 14, Rgb888::BLACK);
        let samples: Vec<Option<f64>> = (0..10).map(|i| Some(i as f64)).collect();
        draw_sparkline(&mut c, 0, 0, 30, 14, &samples, Rgb888::WHITE, None, None);
        assert!(count_lit(&c, Rgb888::WHITE) > 0);
    }

    #[test]
    fn gap_segment_is_skipped() {
        // A middle None splits the line; with a None at the only join, no
        // segment connects past it.
        let mut c = Canvas::new(10, 10, Rgb888::BLACK);
        let samples = vec![Some(0.0), None, Some(9.0)];
        draw_sparkline(
            &mut c,
            0,
            0,
            10,
            10,
            &samples,
            Rgb888::WHITE,
            Some(0.0),
            Some(9.0),
        );
        // Both segments touch a None endpoint, so nothing is stroked.
        assert_eq!(count_lit(&c, Rgb888::WHITE), 0);
    }

    #[test]
    fn pinned_scale_puts_max_at_top() {
        let mut c = Canvas::new(10, 10, Rgb888::BLACK);
        let samples = vec![Some(100.0), Some(100.0)];
        draw_sparkline(
            &mut c,
            0,
            0,
            10,
            10,
            &samples,
            Rgb888::WHITE,
            Some(0.0),
            Some(100.0),
        );
        // The 100 value maps to the top row (y = 0) under a 0..100 pin.
        assert_eq!(c.pixel(0, 0), Rgb888::WHITE);
    }
}
