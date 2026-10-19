//! GNSS detail page (`details.gnss`): fix, satellites, HDOP, PPS lock, the RTK
//! base mode with its survey-in progress, and the dual-antenna heading.
//!
//! Reads [`PageContext::gnss`]; the More menu opens it only when a GNSS
//! service is fitted, and a missing context renders a one-line notice. The
//! label/value row painter here is shared with the radios and LoRa pages.

use embedded_graphics::pixelcolor::Rgb888;

use crate::graphics::bar_meter::draw_bar;
use crate::graphics::fonts::{FontFace, LoadedFont};
use crate::graphics::palette::Palette;
use crate::graphics::primitives::{text, Canvas};
use crate::graphics::status_dot::draw_dot;
use crate::pages::gs_ctx::{fix_label, GnssCtx};
use crate::pages::{blank_panel, Chrome, HitZone, Page, PageContext};
use crate::widgets::{detail_back_zone, draw_detail_header, DETAIL_HEADER_H};

/// Vertical step between rows.
pub(super) const ROW_H: i32 = 24;
/// Left edge of the caps labels.
pub(super) const LABEL_X: i32 = 16;
/// Left edge of the values.
pub(super) const VALUE_X: i32 = 124;
/// Radius of the status dot a row value may lead with.
const DOT_R: i32 = 5;

/// First row's y under the header band.
pub(super) const BODY_Y: i32 = DETAIL_HEADER_H + 8;

/// Paint one caps label + mono value row at `y`. A `dot` colour paints a
/// status dot ahead of the value; `value_color` overrides the primary text.
pub(super) fn draw_kv_row(
    canvas: &mut Canvas,
    palette: &Palette,
    y: i32,
    label: &str,
    value: &str,
    dot: Option<Rgb888>,
    value_color: Option<Rgb888>,
) {
    let label_font = LoadedFont::new(FontFace::SansBold, 11);
    let value_font = LoadedFont::new(FontFace::MonoRegular, 12);
    text(
        canvas,
        &label_font,
        &label.to_ascii_uppercase(),
        LABEL_X,
        y,
        palette.text_tertiary,
    );
    let mut x = VALUE_X;
    if let Some(color) = dot {
        draw_dot(canvas, x + DOT_R, y + 7, color, DOT_R, palette.bg_primary);
        x += DOT_R * 2 + 8;
    }
    text(
        canvas,
        &value_font,
        value,
        x,
        y - 1,
        value_color.unwrap_or(palette.text_primary),
    );
}

/// Paint a centred one-line notice for an absent service.
pub(super) fn draw_absent(canvas: &mut Canvas, palette: &Palette, message: &str) {
    let font = LoadedFont::new(FontFace::SansRegular, 14);
    let (w, _) = font.text_size(message);
    let x = (canvas.width() as i32 - w as i32) / 2;
    text(canvas, &font, message, x, 150, palette.text_secondary);
}

/// `Some(v)` formatted, else `--`.
pub(super) fn or_dash<T>(v: Option<T>, f: impl FnOnce(T) -> String) -> String {
    v.map(f).unwrap_or_else(|| "--".to_string())
}

/// The GNSS detail view, registered as `details.gnss`.
pub struct GnssPage;

impl Page for GnssPage {
    fn id(&self) -> &'static str {
        "details.gnss"
    }

    fn chrome(&self) -> Chrome {
        Chrome::FullScreen
    }

    fn refresh_hz(&self) -> f32 {
        1.0
    }

    fn render(&self, ctx: &PageContext, palette: &Palette) -> Canvas {
        let mut canvas = blank_panel(palette);
        draw_detail_header(&mut canvas, palette, "GNSS");
        match ctx.gnss.as_ref() {
            Some(gnss) => draw_body(&mut canvas, palette, gnss),
            None => draw_absent(&mut canvas, palette, "No GNSS receiver fitted"),
        }
        canvas
    }

    fn hit_zones(&self, _ctx: &PageContext) -> Vec<HitZone> {
        vec![detail_back_zone()]
    }
}

fn draw_body(canvas: &mut Canvas, palette: &Palette, g: &GnssCtx) {
    let mut y = BODY_Y;
    let fix_color = match g.fix.as_deref() {
        Some("none") | None => palette.status_error,
        Some("2d") => palette.status_warning,
        _ => palette.status_success,
    };
    draw_kv_row(
        canvas,
        palette,
        y,
        "Fix",
        fix_label(g.fix.as_deref()),
        Some(fix_color),
        None,
    );
    y += ROW_H;
    let sats = or_dash(g.sats, |s| s.to_string());
    draw_kv_row(canvas, palette, y, "Satellites", &sats, None, None);
    y += ROW_H;
    let hdop = or_dash(g.hdop, |h| format!("{h:.1}"));
    draw_kv_row(canvas, palette, y, "HDOP", &hdop, None, None);
    y += ROW_H;
    let (pps, pps_color) = match g.pps_locked {
        Some(true) => ("LOCKED", Some(palette.status_success)),
        Some(false) => ("UNLOCKED", Some(palette.status_warning)),
        None => ("--", None),
    };
    draw_kv_row(canvas, palette, y, "PPS", pps, pps_color, None);
    y += ROW_H;

    let base = g.base.as_ref();
    let mode = base
        .and_then(|b| b.mode.as_deref())
        .unwrap_or("--")
        .to_ascii_uppercase();
    let state = base.and_then(|b| b.state.as_deref()).unwrap_or("");
    let base_color = match state {
        "fixed" => Some(palette.status_success),
        "surveying" => Some(palette.status_warning),
        "failed" => Some(palette.status_error),
        _ => None,
    };
    draw_kv_row(
        canvas,
        palette,
        y,
        "Base",
        format!("{mode}  {state}").trim_end(),
        base_color,
        None,
    );
    y += ROW_H;
    if let Some(b) = base.filter(|b| b.mode.as_deref() == Some("survey")) {
        let progress = match (b.elapsed_s, b.min_dur_s) {
            (Some(e), Some(m)) => format!("{e:.0} / {m:.0} s"),
            _ => "--".to_string(),
        };
        draw_kv_row(canvas, palette, y, "Survey", &progress, None, None);
        draw_bar(
            canvas,
            VALUE_X + 150,
            y + 2,
            180,
            10,
            b.survey_fraction(),
            10,
            palette.accent_primary,
            palette.bg_tertiary,
            2,
        );
        y += ROW_H;
    }
    let acc = match base.map(|b| (b.accuracy_m, b.target_accuracy_m)) {
        Some((Some(a), Some(t))) => format!("{a:.1} / {t:.1} m"),
        Some((Some(a), None)) => format!("{a:.1} m"),
        _ => "--".to_string(),
    };
    draw_kv_row(canvas, palette, y, "Accuracy", &acc, None, None);
    y += ROW_H;
    let heading = or_dash(g.heading_deg, |h| format!("{h:.1} deg"));
    draw_kv_row(canvas, palette, y, "Heading", &heading, None, None);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::palette::DARK;
    use crate::pages::gs_ctx::GnssBaseCtx;
    use crate::pages::{HitAction, PANEL_W};

    fn has_color(c: &Canvas, color: Rgb888) -> bool {
        (DETAIL_HEADER_H..c.height() as i32)
            .any(|y| (0..PANEL_W as i32).any(|x| c.pixel(x, y) == color))
    }

    fn ctx(fix: &str, pps: bool) -> PageContext {
        PageContext {
            gnss: Some(GnssCtx {
                fix: Some(fix.into()),
                sats: Some(14),
                hdop: Some(0.8),
                pps_locked: Some(pps),
                base: Some(GnssBaseCtx {
                    mode: Some("survey".into()),
                    state: Some("surveying".into()),
                    elapsed_s: Some(120.0),
                    min_dur_s: Some(300.0),
                    accuracy_m: Some(3.1),
                    target_accuracy_m: Some(2.0),
                }),
                heading_deg: None,
            }),
            ..PageContext::default()
        }
    }

    #[test]
    fn renders_with_back_zone() {
        let page = GnssPage;
        let c = page.render(&PageContext::default(), &DARK);
        assert_eq!(c.width(), PANEL_W);
        assert_eq!(
            page.hit_zones(&PageContext::default())[0].action,
            HitAction::Back
        );
        assert_eq!(page.id(), "details.gnss");
    }

    #[test]
    fn a_3d_fix_paints_success_and_survey_progress() {
        let c = GnssPage.render(&ctx("3d", true), &DARK);
        assert!(has_color(&c, DARK.status_success));
        assert!(has_color(&c, DARK.accent_primary), "survey bar");
        assert!(!has_color(&c, DARK.status_error));
    }

    #[test]
    fn no_fix_paints_error() {
        let c = GnssPage.render(&ctx("none", false), &DARK);
        assert!(has_color(&c, DARK.status_error));
    }
}
