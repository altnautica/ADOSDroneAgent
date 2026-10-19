//! Radios detail page (`details.radios`): one row per WFB adapter (index,
//! role, interface, best-chain RSSI, loss, a TX badge on the adapter carrying
//! the transmit lanes) and the receive-diversity summary.
//!
//! Reads [`PageContext::radio_adapters`]; the More menu opens it only when a
//! receive plane reports.

use crate::graphics::fonts::{FontFace, LoadedFont};
use crate::graphics::palette::Palette;
use crate::graphics::primitives::{fill_rect, line, text, Canvas};
use crate::pages::gnss::{draw_absent, or_dash, BODY_Y, LABEL_X};
use crate::pages::gs_ctx::{best_rssi, loss_pct, RadioAdaptersCtx};
use crate::pages::{blank_panel, Chrome, HitZone, Page, PageContext, PANEL_H, PANEL_W};
use crate::widgets::{detail_back_zone, draw_detail_header};

/// Vertical step between adapter rows.
const ROW_H: i32 = 26;
/// Column x positions: index, role, iface, RSSI, loss, TX badge.
const COLS: [i32; 6] = [LABEL_X, 44, 112, 216, 320, 404];
/// Height of the diversity summary band at the bottom.
const FOOTER_H: i32 = 34;

/// The radios detail view, registered as `details.radios`.
pub struct RadiosPage;

impl Page for RadiosPage {
    fn id(&self) -> &'static str {
        "details.radios"
    }

    fn chrome(&self) -> Chrome {
        Chrome::FullScreen
    }

    fn refresh_hz(&self) -> f32 {
        1.0
    }

    fn render(&self, ctx: &PageContext, palette: &Palette) -> Canvas {
        let mut canvas = blank_panel(palette);
        draw_detail_header(&mut canvas, palette, "Radios");
        match ctx.radio_adapters.as_ref() {
            Some(r) => draw_body(&mut canvas, palette, r),
            None => draw_absent(&mut canvas, palette, "No radio receive plane"),
        }
        canvas
    }

    fn hit_zones(&self, _ctx: &PageContext) -> Vec<HitZone> {
        vec![detail_back_zone()]
    }
}

fn draw_body(canvas: &mut Canvas, palette: &Palette, r: &RadioAdaptersCtx) {
    let head_font = LoadedFont::new(FontFace::SansBold, 11);
    let value_font = LoadedFont::new(FontFace::MonoRegular, 12);
    let heads = ["#", "ROLE", "IFACE", "RSSI", "LOSS", ""];
    for (x, h) in COLS.iter().zip(heads) {
        text(canvas, &head_font, h, *x, BODY_Y, palette.text_tertiary);
    }
    let footer_y = PANEL_H as i32 - FOOTER_H;
    let max_rows = ((footer_y - BODY_Y - ROW_H) / ROW_H).max(0) as usize;
    let mut y = BODY_Y + ROW_H;
    for (i, a) in r.adapters.iter().take(max_rows).enumerate() {
        let cells = [
            (i + 1).to_string(),
            a.role.as_str().to_ascii_uppercase(),
            a.iface.clone(),
            or_dash(best_rssi(a), |v| format!("{v} dBm")),
            or_dash(loss_pct(a), |v| format!("{v:.1}%")),
        ];
        for (x, cell) in COLS.iter().zip(cells.iter()) {
            text(canvas, &value_font, cell, *x, y - 1, palette.text_primary);
        }
        if a.tx_active {
            draw_tx_badge(canvas, palette, COLS[5], y - 2);
        }
        y += ROW_H;
    }
    if r.adapters.is_empty() {
        text(
            canvas,
            &value_font,
            "no adapters",
            COLS[0],
            y - 1,
            palette.text_secondary,
        );
    }

    line(
        canvas,
        0,
        footer_y,
        PANEL_W as i32 - 1,
        footer_y,
        palette.border_default,
    );
    let d = &r.diversity;
    let summary = format!(
        "{} rx   combined {}   best single {}",
        d.rx_adapters,
        or_dash(d.combined_loss_pct, |v| format!("{v:.1}%")),
        or_dash(d.best_single_loss_pct, |v| format!("{v:.1}%")),
    );
    text(
        canvas,
        &head_font,
        "DIVERSITY",
        LABEL_X,
        footer_y + 10,
        palette.text_tertiary,
    );
    text(
        canvas,
        &value_font,
        &summary,
        COLS[2],
        footer_y + 9,
        palette.text_primary,
    );
}

/// A filled accent badge reading `TX`.
fn draw_tx_badge(canvas: &mut Canvas, palette: &Palette, x: i32, y: i32) {
    fill_rect(canvas, x, y, x + 30, y + 17, palette.accent_primary);
    let font = LoadedFont::new(FontFace::SansBold, 11);
    text(canvas, &font, "TX", x + 7, y + 2, palette.bg_primary);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::palette::DARK;
    use crate::pages::gs_ctx::{AdapterRole, DiversityReport, RadioAdapterReport};
    use crate::pages::HitAction;

    fn adapter(iface: &str, role: AdapterRole, tx: bool) -> RadioAdapterReport {
        RadioAdapterReport {
            iface: iface.into(),
            mac: None,
            usb_path: None,
            driver: "rtl88x2eu".into(),
            role,
            antenna: None,
            tx_active: tx,
            rssi_dbm: vec![-51, -48],
            packets: 990,
            lost: 10,
            fec_rec: None,
            verified: true,
        }
    }

    fn ctx(tx: bool) -> PageContext {
        PageContext {
            radio_adapters: Some(RadioAdaptersCtx {
                adapters: vec![
                    adapter("wlan1", AdapterRole::Rxtx, tx),
                    adapter("wlan2", AdapterRole::Rx, false),
                ],
                diversity: DiversityReport {
                    rx_adapters: 2,
                    combined_loss_pct: Some(0.1),
                    best_single_loss_pct: Some(1.0),
                },
            }),
            ..PageContext::default()
        }
    }

    fn badge_painted(c: &Canvas) -> bool {
        c.pixel(COLS[5] + 2, BODY_Y + ROW_H) == DARK.accent_primary
    }

    #[test]
    fn renders_with_back_zone() {
        let page = RadiosPage;
        assert_eq!(page.id(), "details.radios");
        let c = page.render(&PageContext::default(), &DARK);
        assert_eq!(c.width(), PANEL_W);
        assert_eq!(
            page.hit_zones(&PageContext::default())[0].action,
            HitAction::Back
        );
    }

    #[test]
    fn the_tx_adapter_carries_the_badge() {
        assert!(badge_painted(&RadiosPage.render(&ctx(true), &DARK)));
        assert!(!badge_painted(&RadiosPage.render(&ctx(false), &DARK)));
    }

    #[test]
    fn loss_and_best_chain() {
        let a = adapter("wlan1", AdapterRole::Rx, false);
        assert_eq!(best_rssi(&a), Some(-48));
        assert_eq!(loss_pct(&a), Some(1.0));
    }
}
