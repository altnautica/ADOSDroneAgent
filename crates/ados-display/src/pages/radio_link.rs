//! Radio Link detail page.
//!
//! The drilldown from the dashboard's radio-link tile. It shows a 60-second
//! RSSI sparkline across the top, a three-column readout grid in the middle
//! (SNR/noise/loss, bitrate/FEC, channel/band), and a TX-power slider with
//! stepper buttons along the bottom.
//!
//! The slider shows the snapshot `tx_power_dbm` clamped into the `1..=15` dBm
//! envelope. The ± stepper buttons write one step up or down through
//! `PUT /api/wfb/tx-power`; the next snapshot shows what the radio accepted.
//! With no current TX power (no snapshot, or a stale one) the steppers are
//! inert and the slider reads `--`, so a tap never sends a step from a guess.

use crate::graphics::fonts::{FontFace, LoadedFont};
use crate::graphics::palette::Palette;
use crate::graphics::primitives::{fill_rect, fill_rect_outline, line, text, Canvas};
use crate::graphics::sparkline::draw_sparkline;
use crate::pages::{
    blank_panel, AgentRequest, Chrome, HitAction, HitZone, Page, PageContext, PanelAction,
};
use crate::widgets::{draw_detail_header, DETAIL_HEADER_H};

/// Layout reference height of the detail-modal surface. The radio-link layout
/// was tuned against a 480x244 content frame; the modal renders into the top of
/// the panel so every y derives from this reference exactly.
const PAGE_W: i32 = 480;

/// Header band height shared by every detail modal.
const HEADER_H: i32 = DETAIL_HEADER_H;

/// TX-power envelope floor in dBm.
const TX_MIN_DBM: i64 = 1;
/// TX-power envelope ceiling in dBm.
const TX_MAX_DBM: i64 = 15;

/// Slider track geometry.
const SLIDER_X: i32 = 60;
const SLIDER_Y: i32 = 200;
const SLIDER_W: i32 = 360;
const SLIDER_TRACK_H: i32 = 8;
const THUMB_W: i32 = 24;
const THUMB_H: i32 = 24;
/// Minus stepper button.
const MINUS_X: i32 = 8;
const MINUS_Y: i32 = 188;
const MINUS_W: i32 = 44;
const MINUS_H: i32 = 44;
/// Plus stepper button.
const PLUS_X: i32 = 428;
const PLUS_Y: i32 = 188;
const PLUS_W: i32 = 44;
const PLUS_H: i32 = 44;

/// The radio-link detail view, registered as `details.radio_link`.
#[derive(Default)]
pub struct RadioLinkDetailPage;

impl RadioLinkDetailPage {
    /// Build a radio-link detail page.
    pub fn new() -> Self {
        Self
    }
}

/// The TX power the radio reports, unclamped. The steppers write inside the
/// `1..=15` dBm envelope, but a value set elsewhere above it is shown as it is.
fn current_tx(ctx: &PageContext) -> Option<i64> {
    ctx.link.tx_power_dbm
}

/// The RSSI trend's `(peak, floor)` in dBm, `None` with no samples. RSSI is
/// negative dBm, so the peak is the least negative sample, never clamped to 0.
fn rssi_peak_floor(history: &[Option<f64>]) -> (Option<i64>, Option<i64>) {
    let real = history.iter().flatten().copied();
    let peak = real.clone().reduce(f64::max).map(|v| v as i64);
    let floor = real.reduce(f64::min).map(|v| v as i64);
    (peak, floor)
}

/// The TX-power write for one stepper tap: `delta` dBm from the current value,
/// clamped into the envelope. `None` when there is no current value.
fn tx_step(ctx: &PageContext, delta: i64) -> Option<PanelAction> {
    let current = current_tx(ctx)?;
    let next = (current + delta).clamp(TX_MIN_DBM, TX_MAX_DBM);
    // A step never moves the wrong way: from above the envelope, `+` would
    // otherwise clamp down to the ceiling.
    if (delta > 0 && next < current) || (delta < 0 && next > current) {
        return None;
    }
    Some(PanelAction::Agent(AgentRequest {
        method: "PUT",
        path: "/api/wfb/tx-power",
        body: Some(serde_json::json!({ "tx_power_dbm": next })),
        label: format!("TX power {next} dBm"),
    }))
}

impl Page for RadioLinkDetailPage {
    fn id(&self) -> &'static str {
        "details.radio_link"
    }

    fn chrome(&self) -> Chrome {
        Chrome::FullScreen
    }

    fn refresh_hz(&self) -> f32 {
        2.0
    }

    fn render(&self, ctx: &PageContext, palette: &Palette) -> Canvas {
        let mut canvas = blank_panel(palette);
        draw_detail_header(&mut canvas, palette, "Radio link");

        let link = &ctx.link;

        // Bitrate canonical key is bitrate_kbps; accept legacy bitrate_mbps so a
        // caller that only knows the heartbeat shape still renders.
        let bitrate_mbps: Option<f64> = match link.bitrate_kbps {
            Some(kbps) if kbps > 0.0 => Some(kbps / 1000.0),
            _ => match link.bitrate_mbps {
                Some(mbps) if mbps > 0.0 => Some(mbps),
                _ => None,
            },
        };

        // Sparkline band y=44..120 (76 px tall, 16 px reserved for the footer).
        let spark_y = HEADER_H + 4;
        let spark_h = 76;
        let (peak, floor) = rssi_peak_floor(&link.rssi_history);
        if !link.rssi_history.is_empty() {
            draw_sparkline(
                &mut canvas,
                8,
                spark_y,
                (PAGE_W - 16) as u32,
                (spark_h - 16) as u32,
                &link.rssi_history,
                palette.accent_primary,
                None,
                None,
            );
        } else {
            let empty_font = LoadedFont::new(FontFace::SansRegular, 11);
            let msg = "no history yet";
            let mw = empty_font.text_size(msg).0 as i32;
            text(
                &mut canvas,
                &empty_font,
                msg,
                (PAGE_W - mw) / 2,
                spark_y + (spark_h - 16) / 2,
                palette.text_tertiary,
            );
        }
        let dbm = |v: Option<i64>| v.map_or_else(|| "--".to_string(), |v| v.to_string());
        let (peak, floor) = (dbm(peak), dbm(floor));

        // Sparkline footer line: rssi value + peak/floor summary.
        let summary_font = LoadedFont::new(FontFace::MonoRegular, 11);
        // Live link mode suffix: "auto:<rung>" / "auto" when the adaptive FEC
        // controller is armed, "manual" when pinned. Empty when the side does
        // not report it (e.g. a receive-only ground station).
        let mode_suffix = match link.adaptive_bitrate_enabled {
            Some(true) => match link.recommended_tier_name.as_deref() {
                Some(rung) => format!("  ·  auto:{rung}"),
                None => "  ·  auto".to_string(),
            },
            Some(false) => "  ·  manual".to_string(),
            None => String::new(),
        };
        let summary = match link.rssi_dbm {
            None if link.is_stale() => "link stale: no current readings".to_string(),
            Some(rssi) => {
                format!(
                    "rssi {} dBm  (peak {peak} / floor {floor}){mode_suffix}",
                    rssi as i64
                )
            }
            None => format!("rssi -- dBm{mode_suffix}"),
        };
        text(
            &mut canvas,
            &summary_font,
            &summary,
            8,
            spark_y + spark_h - 14,
            palette.text_secondary,
        );

        // 3-column readout grid y=124..184.
        let grid_y = 124;
        let col_w = PAGE_W / 3;

        let snr_text = match link.snr_db {
            Some(snr) => format!("{} dB", snr.round() as i64),
            None => "-- dB".to_string(),
        };
        let loss_suffix = match link.loss_percent {
            Some(loss) => format!("  loss {loss:.1}%"),
            None => String::new(),
        };
        let noise_text = match link.noise_dbm {
            Some(noise) => format!("noise {} dBm{loss_suffix}", noise as i64),
            None => format!("noise --{loss_suffix}"),
        };
        draw_column(
            &mut canvas,
            palette,
            0,
            grid_y,
            "SNR",
            &snr_text,
            &noise_text,
        );

        let bitrate_text = match bitrate_mbps {
            Some(b) => format!("{b:.1} Mbps"),
            None => "-- Mbps".to_string(),
        };
        // Prefer the live transmit config (Reed-Solomon ratio + MCS rate) when
        // the side reports it; fall back to the decode counters otherwise (a
        // receive-only ground station carries the counters, not the TX trio).
        let fec_text = match (link.fec_k, link.fec_n) {
            (Some(k), Some(n)) => {
                let mcs = link.mcs_index.map(|m| format!(" M{m}")).unwrap_or_default();
                format!("FEC {k}/{n}{mcs}")
            }
            _ => match (link.fec_recovered, link.fec_lost) {
                (Some(rec), Some(lost)) => format!("FEC R {rec} L {lost}"),
                _ => "FEC -- / --".to_string(),
            },
        };
        draw_column(
            &mut canvas,
            palette,
            col_w,
            grid_y,
            "Bitrate",
            &bitrate_text,
            &fec_text,
        );

        // Treat zero as no-data: the producer's pre-bind defaults are 0 for
        // channel / freq / bw, and rendering "ch 0" implies a real reading.
        let ch_valid = link.channel.map(|c| c > 0).unwrap_or(false);
        let freq_valid = link.frequency_mhz.map(|f| f > 0).unwrap_or(false);
        let bw_valid = link.bandwidth_mhz.map(|b| b > 0).unwrap_or(false);
        let ch_text = if ch_valid {
            format!("ch {}", link.channel.unwrap())
        } else {
            "ch --".to_string()
        };
        let band_text = if freq_valid && bw_valid {
            format!(
                "{} MHz · {} MHz",
                link.frequency_mhz.unwrap(),
                link.bandwidth_mhz.unwrap()
            )
        } else if freq_valid {
            format!("{} MHz", link.frequency_mhz.unwrap())
        } else {
            "-- MHz".to_string()
        };
        draw_column(
            &mut canvas,
            palette,
            col_w * 2,
            grid_y,
            "Channel",
            &ch_text,
            &band_text,
        );

        // TX-power slider y=188..236.
        draw_slider(&mut canvas, palette, current_tx(ctx));
        canvas
    }

    fn hit_zones(&self, ctx: &PageContext) -> Vec<HitZone> {
        let mut zones = vec![HitZone::new(8, 8, 40, 32, HitAction::Back)];
        if current_tx(ctx).is_some() {
            zones.push(HitZone::new(
                MINUS_X,
                MINUS_Y,
                MINUS_W,
                MINUS_H,
                HitAction::Custom("radio.tx_minus".to_string()),
            ));
            zones.push(HitZone::new(
                PLUS_X,
                PLUS_Y,
                PLUS_W,
                PLUS_H,
                HitAction::Custom("radio.tx_plus".to_string()),
            ));
        }
        zones
    }

    fn on_custom(&self, key: &str, ctx: &PageContext) -> Option<PanelAction> {
        match key {
            "radio.tx_minus" => tx_step(ctx, -1),
            "radio.tx_plus" => tx_step(ctx, 1),
            _ => None,
        }
    }
}

/// Paint one readout column: a caps label, a mono-bold value, and a mono
/// secondary line.
fn draw_column(
    canvas: &mut Canvas,
    palette: &Palette,
    x0: i32,
    grid_y: i32,
    label: &str,
    primary: &str,
    secondary: &str,
) {
    let label_font = LoadedFont::new(FontFace::SansBold, 10);
    let value_font = LoadedFont::new(FontFace::MonoBold, 14);
    let unit_font = LoadedFont::new(FontFace::MonoRegular, 11);
    text(
        canvas,
        &label_font,
        &label.to_ascii_uppercase(),
        x0 + 8,
        grid_y,
        palette.text_tertiary,
    );
    text(
        canvas,
        &value_font,
        primary,
        x0 + 8,
        grid_y + 16,
        palette.text_primary,
    );
    text(
        canvas,
        &unit_font,
        secondary,
        x0 + 8,
        grid_y + 36,
        palette.text_secondary,
    );
}

/// Paint the TX-power slider, value chip, and ± stepper buttons.
fn draw_slider(canvas: &mut Canvas, palette: &Palette, value_dbm: Option<i64>) {
    // Stepper button backgrounds.
    for (x0, y0) in [(MINUS_X, MINUS_Y), (PLUS_X, PLUS_Y)] {
        fill_rect_outline(
            canvas,
            x0,
            y0,
            x0 + MINUS_W - 1,
            y0 + MINUS_H - 1,
            palette.bg_secondary,
            palette.border_strong,
        );
    }
    // Minus glyph.
    let center_y_minus = MINUS_Y + MINUS_H / 2;
    let center_y_plus = PLUS_Y + PLUS_H / 2;
    for off in 0..2 {
        line(
            canvas,
            MINUS_X + 12,
            center_y_minus + off,
            MINUS_X + MINUS_W - 12,
            center_y_minus + off,
            palette.text_primary,
        );
        line(
            canvas,
            PLUS_X + 12,
            center_y_plus + off,
            PLUS_X + PLUS_W - 12,
            center_y_plus + off,
            palette.text_primary,
        );
    }
    let cx_plus = PLUS_X + PLUS_W / 2;
    for off in 0..2 {
        line(
            canvas,
            cx_plus + off,
            PLUS_Y + 12,
            cx_plus + off,
            PLUS_Y + PLUS_H - 12,
            palette.text_primary,
        );
    }

    // Track.
    let track_y = SLIDER_Y + (THUMB_H - SLIDER_TRACK_H) / 2;
    fill_rect_outline(
        canvas,
        SLIDER_X,
        track_y,
        SLIDER_X + SLIDER_W - 1,
        track_y + SLIDER_TRACK_H - 1,
        palette.bg_tertiary,
        palette.border_default,
    );

    // With no current value there is no thumb: a thumb parked at the floor
    // would read as a real 1 dBm setting.
    let Some(value_dbm) = value_dbm else {
        let chip_font = LoadedFont::new(FontFace::MonoBold, 11);
        let (cw, ch) = chip_font.text_size("-- dBm");
        text(
            canvas,
            &chip_font,
            "-- dBm",
            SLIDER_X + (SLIDER_W - cw as i32) / 2,
            SLIDER_Y - ch as i32 - 4,
            palette.text_tertiary,
        );
        return;
    };

    // Filled portion to the thumb.
    let clamped = value_dbm.clamp(TX_MIN_DBM, TX_MAX_DBM);
    let frac = (clamped - TX_MIN_DBM) as f64 / (TX_MAX_DBM - TX_MIN_DBM) as f64;
    let thumb_cx = SLIDER_X + (frac * SLIDER_W as f64).round() as i32;
    fill_rect(
        canvas,
        SLIDER_X,
        track_y,
        thumb_cx,
        track_y + SLIDER_TRACK_H - 1,
        palette.accent_primary,
    );

    // Thumb.
    let thumb_x0 = thumb_cx - THUMB_W / 2;
    fill_rect_outline(
        canvas,
        thumb_x0,
        SLIDER_Y,
        thumb_x0 + THUMB_W - 1,
        SLIDER_Y + THUMB_H - 1,
        palette.accent_primary,
        palette.text_primary,
    );

    // Value chip just above the thumb.
    let chip_text = format!("{value_dbm} dBm");
    let chip_font = LoadedFont::new(FontFace::MonoBold, 11);
    let (cw, ch) = chip_font.text_size(&chip_text);
    let chip_y = SLIDER_Y - ch as i32 - 4;
    let chip_x = (thumb_cx - cw as i32 / 2).clamp(SLIDER_X, SLIDER_X + SLIDER_W - cw as i32);
    text(
        canvas,
        &chip_font,
        &chip_text,
        chip_x,
        chip_y,
        palette.text_primary,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::palette::DARK;
    use crate::pages::PANEL_W;

    fn ctx_with_link() -> PageContext {
        let mut ctx = PageContext::default();
        ctx.link.rssi_dbm = Some(-58.0);
        ctx.link.snr_db = Some(24.0);
        ctx.link.noise_dbm = Some(-90.0);
        ctx.link.loss_percent = Some(1.2);
        ctx.link.bitrate_kbps = Some(18000.0);
        ctx.link.fec_recovered = Some(7);
        ctx.link.fec_lost = Some(0);
        ctx.link.fec_k = Some(8);
        ctx.link.fec_n = Some(12);
        ctx.link.mcs_index = Some(3);
        ctx.link.adaptive_bitrate_enabled = Some(true);
        ctx.link.recommended_tier_name = Some("high".to_string());
        ctx.link.channel = Some(149);
        ctx.link.frequency_mhz = Some(5745);
        ctx.link.bandwidth_mhz = Some(20);
        ctx.link.tx_power_dbm = Some(10);
        ctx.link.rssi_history = (0..60).map(|i| Some(-60.0 + (i % 10) as f64)).collect();
        ctx
    }

    #[test]
    fn radio_link_renders_with_zones() {
        let page = RadioLinkDetailPage::new();
        let ctx = ctx_with_link();
        let c = page.render(&ctx, &DARK);
        assert_eq!(c.width(), PANEL_W);
        let zones = page.hit_zones(&ctx);
        assert_eq!(zones.len(), 3);
        assert_eq!(zones[0].action, HitAction::Back);
        assert_eq!(
            zones[1].action,
            HitAction::Custom("radio.tx_minus".to_string())
        );
        assert_eq!(
            zones[2].action,
            HitAction::Custom("radio.tx_plus".to_string())
        );
    }

    /// The steppers write one clamped step through the tx-power route.
    #[test]
    fn steppers_write_one_clamped_step() {
        let page = RadioLinkDetailPage::new();
        let mut ctx = ctx_with_link();
        let body = |key: &str, ctx: &PageContext| match page.on_custom(key, ctx) {
            Some(PanelAction::Agent(req)) => {
                assert_eq!((req.method, req.path), ("PUT", "/api/wfb/tx-power"));
                req.body.expect("a body")["tx_power_dbm"].as_i64()
            }
            other => panic!("{key}: {other:?}"),
        };
        assert_eq!(body("radio.tx_plus", &ctx), Some(11));
        assert_eq!(body("radio.tx_minus", &ctx), Some(9));
        ctx.link.tx_power_dbm = Some(TX_MAX_DBM);
        assert_eq!(body("radio.tx_plus", &ctx), Some(TX_MAX_DBM));
    }

    /// With no current TX power the steppers neither paint a zone nor write.
    #[test]
    fn an_unknown_tx_power_disables_the_steppers() {
        let page = RadioLinkDetailPage::new();
        let ctx = PageContext::default();
        assert_eq!(page.hit_zones(&ctx).len(), 1);
        assert!(page.on_custom("radio.tx_plus", &ctx).is_none());
    }

    /// RSSI is negative dBm: the peak of `[-70, -55]` is -55, not 0.
    #[test]
    fn rssi_peak_is_the_strongest_negative_sample() {
        let history = [Some(-70.0), None, Some(-55.0)];
        assert_eq!(rssi_peak_floor(&history), (Some(-55), Some(-70)));
        assert_eq!(rssi_peak_floor(&[None, None]), (None, None));
    }

    /// A reported TX above the stepper envelope is shown and stepped from as
    /// it is; `+` from there writes nothing rather than lowering it.
    #[test]
    fn tx_above_the_envelope_is_reported_unclamped() {
        let page = RadioLinkDetailPage::new();
        let mut ctx = ctx_with_link();
        ctx.link.tx_power_dbm = Some(20);
        assert_eq!(current_tx(&ctx), Some(20));
        assert!(page.on_custom("radio.tx_plus", &ctx).is_none());
        let Some(PanelAction::Agent(req)) = page.on_custom("radio.tx_minus", &ctx) else {
            panic!("minus steps down");
        };
        assert_eq!(
            req.body.unwrap()["tx_power_dbm"],
            serde_json::json!(TX_MAX_DBM)
        );
    }

    #[test]
    fn empty_history_paints_placeholder() {
        let page = RadioLinkDetailPage::new();
        let ctx = PageContext::default();
        // No history: render must still produce a full panel without panicking.
        let c = page.render(&ctx, &DARK);
        assert_eq!(c.width(), PANEL_W);
    }
}
