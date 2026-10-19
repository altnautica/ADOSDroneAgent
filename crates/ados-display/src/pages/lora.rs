//! LoRa detail page (`details.lora`): region, the governor's transmit state
//! with its blocked reason, one duty meter per `duty_cycle` rule against its
//! limit, power, the refusal total and the newest beacons.
//!
//! The TX state and duty levels come from [`crate::pages::gs_ctx`], the same
//! derivation the OLED `lora` screen reads. Meter colours: under 70 % of the
//! limit the accent, 70-99 % warning, at the limit error.

use crate::graphics::bar_meter::draw_bar;
use crate::graphics::fonts::{FontFace, LoadedFont};
use crate::graphics::palette::Palette;
use crate::graphics::primitives::{text, Canvas};
use crate::pages::gnss::{draw_absent, draw_kv_row, or_dash, BODY_Y, ROW_H, VALUE_X};
use crate::pages::gs_ctx::{Level, LoraCtx, LoraDutyRule};
use crate::pages::{blank_panel, Chrome, HitZone, Page, PageContext};
use crate::widgets::{detail_back_zone, draw_detail_header};

/// Duty meters shown (the plan's first rules; most plans declare one).
const DUTY_ROWS: usize = 2;
/// Duty meter geometry, right of the label column.
const METER_W: u32 = 170;
const METER_H: u32 = 10;

/// The LoRa detail view, registered as `details.lora`.
pub struct LoraPage;

impl Page for LoraPage {
    fn id(&self) -> &'static str {
        "details.lora"
    }

    fn chrome(&self) -> Chrome {
        Chrome::FullScreen
    }

    fn refresh_hz(&self) -> f32 {
        1.0
    }

    fn render(&self, ctx: &PageContext, palette: &Palette) -> Canvas {
        let mut canvas = blank_panel(palette);
        draw_detail_header(&mut canvas, palette, "LoRa");
        match ctx.lora.present() {
            Some(l) => draw_body(&mut canvas, palette, l),
            None => {
                let msg = ctx.lora.missing_text("LoRa module").unwrap_or_default();
                draw_absent(&mut canvas, palette, &msg);
            }
        }
        canvas
    }

    fn hit_zones(&self, _ctx: &PageContext) -> Vec<HitZone> {
        vec![detail_back_zone()]
    }
}

fn draw_body(canvas: &mut Canvas, palette: &Palette, l: &LoraCtx) {
    let mut y = BODY_Y;
    let region = match &l.region_code {
        Some(code) => [
            Some(code.as_str()),
            l.region_name.as_deref(),
            l.plan_id.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("  "),
        None => "--  not set".to_string(),
    };
    draw_kv_row(canvas, palette, y, "Region", &region, None, None);
    y += ROW_H;

    let state = l.tx_state();
    let color = state.level().color(palette);
    draw_kv_row(
        canvas,
        palette,
        y,
        "TX",
        &state.short(),
        Some(color),
        (state.level() != Level::Ok).then_some(color),
    );
    y += ROW_H;
    if let Some(reason) = state.reason() {
        let font = LoadedFont::new(FontFace::SansRegular, 12);
        text(canvas, &font, reason, VALUE_X + 18, y - 4, color);
        y += ROW_H - 4;
    }

    for rule in l.duty_rules.iter().take(DUTY_ROWS) {
        draw_duty_row(canvas, palette, y, rule);
        y += ROW_H;
    }

    let power = match (l.erp_mw, l.max_erp_mw) {
        (Some(e), Some(m)) => format!("{e:.0} / {m:.0} mW ERP"),
        (Some(e), None) => format!("{e:.0} mW ERP"),
        _ => "--".to_string(),
    };
    draw_kv_row(canvas, palette, y, "Power", &power, None, None);
    y += ROW_H;
    let refused = or_dash(l.refused, |r| r.to_string());
    draw_kv_row(canvas, palette, y, "Refused", &refused, None, None);
    y += ROW_H;

    let heard = format!("{} heard", l.beacons_heard);
    draw_kv_row(canvas, palette, y, "Beacons", &heard, None, None);
    y += ROW_H - 4;
    let font = LoadedFont::new(FontFace::MonoRegular, 12);
    for b in &l.beacons {
        let line = format!(
            "node {:<5} {:>5} dBm {:>5} dB {:>5}",
            b.node_id,
            or_dash(b.rssi_dbm, |v| format!("{v:.0}")),
            or_dash(b.snr_db, |v| format!("{v:.1}")),
            or_dash(b.age_s, |s| format!("{s}s")),
        );
        text(canvas, &font, &line, VALUE_X, y, palette.text_secondary);
        y += ROW_H - 6;
    }
}

/// One duty meter row: caps label, the meter coloured by its level, and the
/// used/limit figures.
fn draw_duty_row(canvas: &mut Canvas, palette: &Palette, y: i32, rule: &LoraDutyRule) {
    let level = rule.level();
    let fill = level.map_or(palette.accent_primary, |lv| lv.color(palette));
    draw_kv_row(canvas, palette, y, "Duty", "", None, None);
    draw_bar(
        canvas,
        VALUE_X,
        y + 2,
        METER_W,
        METER_H,
        Some(rule.fraction()),
        20,
        fill,
        palette.bg_tertiary,
        1,
    );
    let figures = format!("{:.2}/{:.1}%", rule.used_pct, rule.limit_pct);
    let font = LoadedFont::new(FontFace::MonoRegular, 12);
    text(
        canvas,
        &font,
        &figures,
        VALUE_X + METER_W as i32 + 10,
        y - 1,
        level.map_or(palette.text_primary, |lv| lv.color(palette)),
    );
    if let Some(band) = rule.sub_band.as_deref() {
        let small = LoadedFont::new(FontFace::SansRegular, 10);
        text(canvas, &small, band, 16 + 40, y + 1, palette.text_tertiary);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::palette::DARK;
    use crate::pages::gs_ctx::LoraBeacon;
    use crate::pages::{HitAction, PANEL_W};
    use embedded_graphics::pixelcolor::Rgb888;

    fn has_color(c: &Canvas, color: Rgb888) -> bool {
        (BODY_Y..c.height() as i32).any(|y| (0..PANEL_W as i32).any(|x| c.pixel(x, y) == color))
    }

    fn lora(reason: Option<&str>, used: f64) -> PageContext {
        PageContext {
            lora: crate::pages::reading::Reading::Present(LoraCtx {
                region_code: reason
                    .filter(|r| *r == "region_unset")
                    .map_or(Some("IN".into()), |_| None),
                region_name: Some("India".into()),
                plan_id: Some("IN865".into()),
                tx_allowed: reason.is_none(),
                blocked_reason: reason.map(str::to_string),
                next_tx_allowed_ms: 462_000,
                duty_rules: vec![LoraDutyRule {
                    sub_band: Some("865.0-867.0".into()),
                    used_pct: used,
                    limit_pct: 1.0,
                }],
                erp_mw: Some(25.0),
                max_erp_mw: Some(25.0),
                refused: Some(0),
                beacons_heard: 1,
                beacons: vec![LoraBeacon {
                    node_id: 42,
                    rssi_dbm: Some(-97.0),
                    snr_db: Some(6.5),
                    age_s: Some(12),
                }],
            }),
            ..PageContext::default()
        }
    }

    #[test]
    fn renders_with_back_zone() {
        let page = LoraPage;
        assert_eq!(page.id(), "details.lora");
        let c = page.render(&PageContext::default(), &DARK);
        assert_eq!(c.width(), PANEL_W);
        assert_eq!(
            page.hit_zones(&PageContext::default())[0].action,
            HitAction::Back
        );
    }

    #[test]
    fn normal_state_paints_success_not_error() {
        let c = LoraPage.render(&lora(None, 0.4), &DARK);
        assert!(has_color(&c, DARK.status_success));
        assert!(has_color(&c, DARK.accent_primary), "neutral duty meter");
        assert!(!has_color(&c, DARK.status_error));
        assert!(!has_color(&c, DARK.status_warning));
    }

    #[test]
    fn blocked_states_paint_the_error_token() {
        for reason in ["region_unset", "module_band_mismatch", "power_over_limit"] {
            let c = LoraPage.render(&lora(Some(reason), 0.1), &DARK);
            assert!(has_color(&c, DARK.status_error), "{reason}");
        }
    }

    #[test]
    fn duty_wait_paints_warning_and_meter_at_limit_error() {
        let c = LoraPage.render(&lora(Some("duty_exhausted"), 1.0), &DARK);
        assert!(has_color(&c, DARK.status_warning), "TX WAIT dot");
        assert!(has_color(&c, DARK.status_error), "meter at limit");
        let c = LoraPage.render(&lora(None, 0.8), &DARK);
        assert!(has_color(&c, DARK.status_warning), "meter over 70 %");
    }
}
