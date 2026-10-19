//! Sensing context for the cameras, sensors and storage pages, plus the small
//! row painters those three pages share.
//!
//! Every block is a [`Reading`]: absent (not fitted or `present: false`),
//! unreachable (the read failed) or present. Pages render "not fitted" or
//! "unreachable" for a missing block and `--` for a missing field.

use embedded_graphics::pixelcolor::Rgb888;

use crate::graphics::fonts::{FontFace, LoadedFont};
use crate::graphics::palette::Palette;
use crate::graphics::primitives::{fill_circle, fill_rect, line, text, Canvas};
use crate::graphics::thresholds::SENSOR_RATE_PCT;
use crate::pages::reading::Reading;
use crate::pages::{PageContext, CONTENT_H, CONTENT_Y, PANEL_W};
use crate::widgets::draw_top_bar;

/// Sensor hub link state (`/api/v1/sensors` `hub`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HubInfo {
    pub connected: bool,
    pub transport: Option<String>,
    pub fw: Option<String>,
    pub board: Option<String>,
    pub crc_errors: Option<i64>,
    pub dropped: Option<i64>,
}

/// One sensor stream (`/api/v1/sensors` `sensors[]`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SensorInfo {
    pub id: String,
    pub kind: String,
    pub role: Option<String>,
    pub source: Option<String>,
    pub rate_hz: Option<f64>,
    pub expected_hz: Option<f64>,
    pub age_ms: Option<f64>,
    pub sigma_ns: Option<f64>,
}

impl SensorInfo {
    /// Measured rate as a percent of expected; `None` without an expected rate.
    pub fn rate_pct(&self) -> Option<f64> {
        match (self.rate_hz, self.expected_hz) {
            (Some(r), Some(e)) if e > 0.0 => Some(r / e * 100.0),
            _ => None,
        }
    }
}

/// GNSS summary (`/api/v1/sensors` `gnss`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GnssSummary {
    pub fix: Option<String>,
    pub sats: Option<i64>,
}

/// `/api/v1/sensors` as the pages read it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SensorsInfo {
    pub hub: Option<HubInfo>,
    /// `fc.connected`; `None` when no FC is configured.
    pub fc_connected: Option<bool>,
    pub sensors: Vec<SensorInfo>,
    pub gnss: Option<GnssSummary>,
}

/// `/api/v1/clock` as the pages read it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClockInfo {
    pub timebase: Option<String>,
    pub locked: bool,
    /// Negative wire values (unknown) map to `None`.
    pub sigma_ns: Option<f64>,
}

/// `/api/v1/storage` as the pages read it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StorageInfo {
    pub capture_root: Option<String>,
    /// Free / total bytes of the capture volume.
    pub free_bytes: Option<f64>,
    pub total_bytes: Option<f64>,
    pub used_bytes: Option<f64>,
    pub budget_bytes: Option<f64>,
    pub sessions: Option<i64>,
    pub uploaded: Option<i64>,
    /// `ok | near_full | full`.
    pub state: Option<String>,
    pub write_mbps: Option<f64>,
}

impl StorageInfo {
    /// Capture bytes used as a percent of the capture budget.
    pub fn budget_use_pct(&self) -> Option<f64> {
        match (self.used_bytes, self.budget_bytes) {
            (Some(u), Some(b)) if b > 0.0 => Some(u / b * 100.0),
            _ => None,
        }
    }

    /// Sessions not yet uploaded.
    pub fn pending_upload(&self) -> Option<i64> {
        Some((self.sessions? - self.uploaded?).max(0))
    }
}

/// One camera row of the capture state (`frames.per_camera[]`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CaptureCamera {
    pub id: String,
    pub fps: Option<f64>,
    pub dropped: Option<i64>,
}

/// Capture exposure (`exposure`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExposureInfo {
    pub locked: bool,
    pub applied: bool,
    pub exposure_us: Option<f64>,
    pub gain: Option<f64>,
    pub mode: Option<String>,
}

/// World-engine capture state as the pages read it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CaptureInfo {
    pub state: Option<String>,
    pub per_camera: Vec<CaptureCamera>,
    pub exposure: Option<ExposureInfo>,
}

/// Everything the sensing pages read. Each block is a [`Reading`]: absent
/// (not fitted, or not read on this profile), unreachable (the read failed)
/// or present.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SensingCtx {
    pub sensors: Reading<SensorsInfo>,
    pub clock: Reading<ClockInfo>,
    pub storage: Reading<StorageInfo>,
    pub capture: Reading<CaptureInfo>,
}

// ── formatting ──────────────────────────────────────────────────────

/// The placeholder for an unknown value.
pub const DASH: &str = "--";

/// Format an optional number with `decimals` places, `--` when absent.
pub fn fmt_num(v: Option<f64>, decimals: usize) -> String {
    v.map_or_else(|| DASH.to_string(), |v| format!("{v:.decimals$}"))
}

/// An optional string or `--`.
pub fn fmt_str(v: Option<&str>) -> String {
    v.filter(|s| !s.is_empty()).unwrap_or(DASH).to_string()
}

/// Colour for a rate verdict: neutral without an expected rate.
pub fn rate_color(palette: &Palette, s: &SensorInfo) -> Rgb888 {
    match s.rate_pct() {
        Some(p) => palette.grade(Some(p), SENSOR_RATE_PCT),
        None => palette.text_primary,
    }
}

/// Colour for a boolean link/lock state.
pub fn bool_color(palette: &Palette, v: Option<bool>) -> Rgb888 {
    match v {
        Some(true) => palette.status_success,
        Some(false) => palette.status_error,
        None => palette.text_tertiary,
    }
}

// ── shared painters ─────────────────────────────────────────────────

/// Panel-global y of the content region top.
pub const OY: i32 = CONTENT_Y as i32;
/// Content-region width.
pub const PAGE_W: i32 = PANEL_W as i32;
/// Header strip height.
pub const HEADER_H: i32 = 24;
/// Row pitch below the header.
pub const ROW_H: i32 = 24;
/// Left edge of row labels.
pub const LABEL_X: i32 = 12;
/// Left edge of row values.
pub const VALUE_X: i32 = 132;

/// Paint the top bar, ground the content region and paint the header strip.
/// The bottom tab band is left untouched for the navigator.
pub fn paint_frame(canvas: &mut Canvas, ctx: &PageContext, palette: &Palette, title: &str) {
    draw_top_bar(
        canvas,
        palette,
        &ctx.hostname,
        ctx.role.current.as_deref().unwrap_or("unset"),
        ctx.system.cpu_pct,
        ctx.system.ram_used_mb,
        ctx.system.ram_total_mb,
        ctx.system.temp_c,
        &ctx.clock,
    );
    let bottom = OY + CONTENT_H as i32 - 1;
    fill_rect(canvas, 0, OY, PAGE_W - 1, bottom, palette.bg_primary);
    fill_rect(
        canvas,
        0,
        OY,
        PAGE_W - 1,
        OY + HEADER_H - 1,
        palette.bg_secondary,
    );
    line(
        canvas,
        0,
        OY + HEADER_H - 1,
        PAGE_W - 1,
        OY + HEADER_H - 1,
        palette.border_default,
    );
    let f = LoadedFont::new(FontFace::SansBold, 11);
    text(canvas, &f, title, LABEL_X, OY + 6, palette.text_tertiary);
}

/// Panel-global y of row `i` below the header.
pub fn row_y(i: i32) -> i32 {
    OY + HEADER_H + 6 + i * ROW_H
}

/// Paint a caps label at row `i`.
pub fn paint_label(canvas: &mut Canvas, palette: &Palette, i: i32, label: &str) {
    let f = LoadedFont::new(FontFace::SansBold, 11);
    text(
        canvas,
        &f,
        label,
        LABEL_X,
        row_y(i) + 2,
        palette.text_secondary,
    );
}

/// Paint a mono value at `(x, row i)`.
pub fn paint_value(canvas: &mut Canvas, i: i32, x: i32, value: &str, color: Rgb888) {
    let f = LoadedFont::new(FontFace::MonoRegular, 12);
    text(canvas, &f, value, x, row_y(i), color);
}

/// Paint a status dot at `(x, row i)` followed by a value in the same colour.
pub fn paint_status(canvas: &mut Canvas, i: i32, x: i32, value: &str, color: Rgb888) {
    fill_circle(canvas, x + 4, row_y(i) + 7, 4, color, None);
    paint_value(canvas, i, x + 14, value, color);
}

/// Paint a centred one-line notice (absent or unreachable) as the body.
pub fn paint_absent(canvas: &mut Canvas, palette: &Palette, msg: &str) {
    let f = LoadedFont::new(FontFace::SansRegular, 14);
    let w = f.text_advance(msg) as i32;
    text(
        canvas,
        &f,
        msg,
        (PAGE_W - w) / 2,
        OY + CONTENT_H as i32 / 2,
        palette.text_tertiary,
    );
}

#[cfg(test)]
pub(crate) mod test_util {
    use super::*;
    use crate::graphics::palette::DARK;
    use crate::pages::{Page, BOTTOM_BAR_H, PANEL_H};

    /// A context with every sensing block filled, imu0 running at `imu_hz`.
    pub fn full_ctx(imu_hz: f64) -> PageContext {
        let sensing = SensingCtx {
            sensors: Reading::Present(SensorsInfo {
                hub: Some(HubInfo {
                    connected: true,
                    transport: Some("serial:/dev/ttyACM0".into()),
                    fw: Some("ados-sensorhub 0.1.0".into()),
                    ..HubInfo::default()
                }),
                fc_connected: Some(true),
                sensors: vec![
                    SensorInfo {
                        id: "imu0".into(),
                        kind: "imu".into(),
                        rate_hz: Some(imu_hz),
                        expected_hz: Some(1000.0),
                        ..SensorInfo::default()
                    },
                    SensorInfo {
                        id: "cam_fl".into(),
                        kind: "camera".into(),
                        role: Some("front_left".into()),
                        source: Some("vision".into()),
                        rate_hz: Some(20.0),
                        expected_hz: Some(20.0),
                        age_ms: Some(9.0),
                        ..SensorInfo::default()
                    },
                ],
                gnss: Some(GnssSummary {
                    fix: Some("3d".into()),
                    sats: Some(14),
                }),
            }),
            clock: Reading::Present(ClockInfo {
                timebase: Some("hub_freerun".into()),
                locked: true,
                sigma_ns: Some(420000.0),
            }),
            storage: Reading::Present(StorageInfo {
                capture_root: Some("/mnt/nvme0/ados/capture".into()),
                free_bytes: Some(8.0e11),
                total_bytes: Some(1.0e12),
                used_bytes: Some(1.0e11),
                budget_bytes: Some(8.0e11),
                sessions: Some(12),
                uploaded: Some(9),
                state: Some("ok".into()),
                write_mbps: Some(1450.2),
            }),
            capture: Reading::Present(CaptureInfo {
                state: Some("recording".into()),
                per_camera: vec![CaptureCamera {
                    id: "cam_fl".into(),
                    fps: Some(20.0),
                    dropped: Some(3),
                }],
                exposure: Some(ExposureInfo {
                    locked: true,
                    applied: false,
                    exposure_us: Some(4000.0),
                    gain: Some(2.0),
                    mode: Some("walking".into()),
                }),
            }),
        };
        PageContext {
            sensing,
            ..PageContext::default()
        }
    }

    /// Render with full and empty ctx and check the bottom band is background.
    pub fn assert_renders_and_leaves_tab_band(page: &dyn Page) {
        let unreachable = PageContext {
            sensing: SensingCtx {
                sensors: Reading::Unreachable,
                clock: Reading::Unreachable,
                storage: Reading::Unreachable,
                capture: Reading::Unreachable,
            },
            ..PageContext::default()
        };
        for ctx in [full_ctx(1000.0), PageContext::default(), unreachable] {
            let c = page.render(&ctx, &DARK);
            assert_eq!((c.width(), c.height()), (PANEL_W, PANEL_H));
            for y in (PANEL_H - BOTTOM_BAR_H) as i32..PANEL_H as i32 {
                for x in 0..PANEL_W as i32 {
                    assert_eq!(c.pixel(x, y), DARK.bg_primary, "tab band at ({x},{y})");
                }
            }
        }
    }

    /// Whether any pixel in row band `i` from `x0` has exactly `color`.
    pub fn row_has_color(c: &Canvas, i: i32, x0: i32, color: Rgb888) -> bool {
        let y0 = row_y(i);
        (y0..y0 + ROW_H - 4).any(|y| (x0..PAGE_W).any(|x| c.pixel(x, y) == color))
    }
}
