//! Sensors page: sensor hub link, IMU rate, clock lock, GNSS and FC link, then
//! the navigation sensors (optical flow, downward range, lidar scan rate and
//! driver health, camera states) from the sensors route's `health`.
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

/// Camera words fit side by side on the last row.
const CAMERA_SLOTS: usize = 3;
/// Width of one camera word on that row.
const CAMERA_SLOT_W: i32 = 116;

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

        // The navigation sensors, as the cockpit spec words and colours them;
        // rows only when the route carries `health`.
        if let Some(h) = sensors.and_then(|x| x.health.as_ref()) {
            let lidar = match h.lidar_health.as_deref() {
                Some(d) => format!("{}  {d}", h.lidar.text),
                None => h.lidar.text.clone(),
            };
            for (row, label, text, color) in [
                (5, "FLOW", h.flow.text.as_str(), h.flow.color),
                (6, "RANGE", h.range.text.as_str(), h.range.color),
                (7, "LIDAR", lidar.as_str(), h.lidar.color),
            ] {
                paint_label(c, palette, row, label);
                paint_status(c, row, VALUE_X, text, color);
            }
            if !h.cameras.is_empty() {
                paint_label(c, palette, 8, "CAMERAS");
                for (i, w) in h.cameras.iter().take(CAMERA_SLOTS).enumerate() {
                    let x = VALUE_X + i as i32 * CAMERA_SLOT_W;
                    paint_status(c, 8, x, &w.text, w.color);
                }
            }
        }
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
    fn flow_range_and_lidar_rows_take_the_spec_words_and_colours() {
        use crate::pages::sensor_health::nav_health;
        use embedded_graphics::pixelcolor::Rgb888;

        let mut ctx = full_ctx(1000.0);
        let health = nav_health(&serde_json::json!({
            "flow": {"state": "ok", "quality": 212},
            "range": {"state": "lost"},
            "lidar": {"state": "degraded", "rate_hz": 5.2, "health": "warning"},
        }))
        .expect("an object");
        assert_eq!(health.flow.text, "Flow 212");
        assert_eq!(health.range.text, "Range lost");
        assert_eq!(health.lidar.text, "Lidar 5.2 Hz");
        let none = SensorsPage.render(&ctx, &DARK);
        if let Some(s) = ctx.sensing.sensors.present_mut() {
            s.health = Some(health);
        }
        let c = SensorsPage.render(&ctx, &DARK);
        let (ready, not_ready, avoiding) = (
            Rgb888::new(0x00, 0xC8, 0x53),
            Rgb888::new(0xEF, 0x44, 0x44),
            Rgb888::new(0xF5, 0x9E, 0x0B),
        );
        assert!(row_has_color(&c, 5, VALUE_X, ready));
        assert!(row_has_color(&c, 6, VALUE_X, not_ready));
        assert!(row_has_color(&c, 7, VALUE_X, avoiding));
        assert!(!row_has_color(&none, 7, VALUE_X, avoiding));
    }

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

    #[test]
    fn camera_states_paint_in_their_spec_colours() {
        use crate::pages::sensor_health::nav_health;
        use embedded_graphics::pixelcolor::Rgb888;

        let mut ctx = full_ctx(1000.0);
        let none = SensorsPage.render(&ctx, &DARK);
        let health = nav_health(&serde_json::json!({"flow": null, "cameras": [
            {"id": "front", "state": "live", "fps": 30.0},
            {"id": "belly", "state": "lost"},
        ]}))
        .expect("an object");
        assert_eq!(health.cameras[1].text, "belly lost");
        assert_eq!(health.flow.text, "—");
        if let Some(s) = ctx.sensing.sensors.present_mut() {
            s.health = Some(health);
        }
        let c = SensorsPage.render(&ctx, &DARK);
        let (ready, not_ready) = (Rgb888::new(0x00, 0xC8, 0x53), Rgb888::new(0xEF, 0x44, 0x44));
        assert!(row_has_color(&c, 8, VALUE_X, ready));
        assert!(row_has_color(&c, 8, VALUE_X + CAMERA_SLOT_W, not_ready));
        assert!(!row_has_color(&none, 8, VALUE_X, ready));
    }
}
