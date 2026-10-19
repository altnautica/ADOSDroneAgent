//! Sensors page: sensor hub link, IMU rate, clock lock, GNSS and FC link.
//!
//! A tabbed read-only surface over [`PageContext::sensing`]. A block that is
//! not fitted reads "not fitted" (or `--`); a failed read reads "unreachable".

use crate::graphics::palette::Palette;
use crate::graphics::primitives::Canvas;
use crate::pages::reading::Reading;
use crate::pages::sensing_ctx::{
    bool_color, fmt_num, fmt_str, paint_absent, paint_frame, paint_label, paint_status, rate_color,
    SensorInfo, DASH, VALUE_X,
};
use crate::pages::{blank_panel, Chrome, HitZone, Page, PageContext};

/// The sensors surface, registered as `sensors`.
#[derive(Debug, Default, Clone, Copy)]
pub struct SensorsPage;

/// `rate / expected Hz`, or just `rate Hz` without an expected rate.
pub(crate) fn rate_text(s: &SensorInfo) -> String {
    match s.expected_hz {
        Some(e) => format!("{} / {} Hz", fmt_num(s.rate_hz, 0), fmt_num(Some(e), 0)),
        None => format!("{} Hz", fmt_num(s.rate_hz, 0)),
    }
}

impl Page for SensorsPage {
    fn id(&self) -> &'static str {
        "sensors"
    }

    fn chrome(&self) -> Chrome {
        Chrome::Tabbed
    }

    fn refresh_hz(&self) -> f32 {
        1.0
    }

    fn render(&self, ctx: &PageContext, palette: &Palette) -> Canvas {
        let mut canvas = blank_panel(palette);
        paint_frame(&mut canvas, ctx, palette, "SENSORS");
        let s = &ctx.sensing;
        if !s.sensors.is_present() && !s.clock.is_present() {
            let failed = matches!(s.sensors, Reading::Unreachable)
                || matches!(s.clock, Reading::Unreachable);
            let msg = if failed {
                "Sensor hub unreachable"
            } else {
                "No sensor hub fitted"
            };
            paint_absent(&mut canvas, palette, msg);
            return canvas;
        }
        let c = &mut canvas;
        let sensors = s.sensors.present();
        // The text for a block the sensors read did not carry.
        let missing = if matches!(s.sensors, Reading::Unreachable) {
            "unreachable"
        } else {
            "not fitted"
        };

        paint_label(c, palette, 0, "HUB");
        match sensors.and_then(|x| x.hub.as_ref()) {
            Some(h) => {
                let state = if h.connected { "up" } else { "down" };
                let v = format!(
                    "{state}  {}  {}",
                    fmt_str(h.transport.as_deref()),
                    fmt_str(h.fw.as_deref())
                );
                paint_status(c, 0, VALUE_X, &v, bool_color(palette, Some(h.connected)));
            }
            None => paint_status(c, 0, VALUE_X, missing, palette.text_tertiary),
        }

        paint_label(c, palette, 1, "IMU");
        match sensors.and_then(|x| x.sensors.iter().find(|s| s.kind == "imu")) {
            Some(imu) => paint_status(c, 1, VALUE_X, &rate_text(imu), rate_color(palette, imu)),
            None => paint_status(c, 1, VALUE_X, DASH, palette.text_tertiary),
        }

        paint_label(c, palette, 2, "CLOCK");
        match s.clock.present() {
            Some(k) => {
                let state = if k.locked { "locked" } else { "free" };
                let sigma = fmt_num(k.sigma_ns.map(|n| n / 1.0e6), 2);
                let v = format!("{state}  σ {sigma} ms  {}", fmt_str(k.timebase.as_deref()));
                paint_status(c, 2, VALUE_X, &v, bool_color(palette, Some(k.locked)));
            }
            None if matches!(s.clock, Reading::Unreachable) => {
                paint_status(c, 2, VALUE_X, "unreachable", palette.text_tertiary)
            }
            None => paint_status(c, 2, VALUE_X, DASH, palette.text_tertiary),
        }

        paint_label(c, palette, 3, "GNSS");
        match sensors.and_then(|x| x.gnss.as_ref()) {
            Some(g) => {
                let fixed = g.fix.as_deref().is_some_and(|f| f != "none");
                let sats = g.sats.map_or_else(|| DASH.to_string(), |n| n.to_string());
                let v = format!("{}  {sats} sats", fmt_str(g.fix.as_deref()));
                paint_status(c, 3, VALUE_X, &v, bool_color(palette, Some(fixed)));
            }
            None => paint_status(c, 3, VALUE_X, missing, palette.text_tertiary),
        }

        paint_label(c, palette, 4, "FC");
        let fc = sensors.and_then(|x| x.fc_connected);
        let v = match fc {
            Some(true) => "connected",
            Some(false) => "disconnected",
            None => DASH,
        };
        paint_status(c, 4, VALUE_X, v, bool_color(palette, fc));
        canvas
    }

    fn hit_zones(&self, _ctx: &PageContext) -> Vec<HitZone> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::palette::DARK;
    use crate::pages::sensing_ctx::test_util::{
        assert_renders_and_leaves_tab_band, full_ctx, row_has_color,
    };

    #[test]
    fn renders_full_and_empty_without_touching_the_tab_band() {
        assert_eq!(SensorsPage.id(), "sensors");
        assert_eq!(SensorsPage.chrome(), Chrome::Tabbed);
        assert_renders_and_leaves_tab_band(&SensorsPage);
    }

    #[test]
    fn degraded_imu_rate_reads_warn_then_bad() {
        let c = SensorsPage.render(&full_ctx(1000.0), &DARK);
        assert!(row_has_color(&c, 1, VALUE_X, DARK.status_success));
        let c = SensorsPage.render(&full_ctx(700.0), &DARK);
        assert!(row_has_color(&c, 1, VALUE_X, DARK.status_warning));
        assert!(!row_has_color(&c, 1, VALUE_X, DARK.status_success));
        let c = SensorsPage.render(&full_ctx(300.0), &DARK);
        assert!(row_has_color(&c, 1, VALUE_X, DARK.status_error));
    }

    #[test]
    fn no_expected_rate_means_no_verdict() {
        let mut ctx = full_ctx(10.0);
        if let Some(s) = ctx.sensing.sensors.present_mut() {
            s.sensors[0].expected_hz = None;
        }
        let c = SensorsPage.render(&ctx, &DARK);
        assert!(!row_has_color(&c, 1, VALUE_X, DARK.status_error));
        assert!(row_has_color(&c, 1, VALUE_X, DARK.text_primary));
    }

    #[test]
    fn unreachable_reads_differently_from_not_fitted() {
        let absent = SensorsPage.render(&PageContext::default(), &DARK);
        let ctx = PageContext {
            sensing: crate::pages::sensing_ctx::SensingCtx {
                sensors: Reading::Unreachable,
                ..Default::default()
            },
            ..PageContext::default()
        };
        let failed = SensorsPage.render(&ctx, &DARK);
        assert_ne!(absent.as_rgb888(), failed.as_rgb888());
    }
}
