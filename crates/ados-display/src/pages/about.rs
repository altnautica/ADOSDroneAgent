//! About page — read-only system identity drilldown.
//!
//! Shown from the settings list as the bottom-most row. The body lists the
//! agent version, board name, device id, device name, the eth0 / wlan0 MAC
//! addresses, a release-time build stamp, the license, and the repo URL. Every
//! value is read-only; the operator backs out via the header chevron.
//!
//! The label/value rows are a fixed table: an uppercase caps label in the left
//! column (Sans Bold 11) and a mono value in the right column (Mono Regular 12),
//! 22 px per row. Missing identity fields render as `--`.
//!
//! All values come from [`PageContext::device`], which the render loop fills
//! from the setup-status snapshot and the HAL board detect plus the sysfs MAC
//! and build-stamp reads.

use crate::graphics::fonts::{FontFace, LoadedFont};
use crate::graphics::palette::Palette;
use crate::graphics::primitives::{text, Canvas};
use crate::pages::{blank_panel, HitZone, Page, PageContext};
use crate::widgets::{detail_back_zone, draw_detail_header, DETAIL_HEADER_H};

/// Header band height shared by every detail modal.
const HEADER_H: i32 = DETAIL_HEADER_H;
/// Vertical step between identity rows.
const ROW_H: i32 = 22;
/// Left edge of the caps labels.
const LABEL_X: i32 = 16;
/// Left edge of the mono values.
const VALUE_X: i32 = 140;
/// Repo URL shown on the last row.
const REPO_URL: &str = "github.com/altnautica/ADOSDroneAgent";

/// Resolve an optional identity field to its display text, defaulting to `--`
/// for an absent or empty value.
fn field(value: &Option<String>) -> String {
    match value {
        Some(s) if !s.is_empty() => s.clone(),
        _ => "--".to_string(),
    }
}

/// The about detail view, registered as `details.about`.
pub struct AboutDetailPage;

impl Page for AboutDetailPage {
    fn id(&self) -> &'static str {
        "details.about"
    }

    fn refresh_hz(&self) -> f32 {
        1.0
    }

    fn render(&self, ctx: &PageContext, palette: &Palette) -> Canvas {
        let mut canvas = blank_panel(palette);
        draw_detail_header(&mut canvas, palette, "About");

        let device = &ctx.device;
        let rows: [(&str, String); 9] = [
            ("Agent", field(&device.version)),
            ("Board", field(&device.board_name)),
            ("Device ID", field(&device.device_id)),
            ("Device", field(&device.device_name)),
            ("eth0", field(&device.mac_eth0)),
            ("wlan0", field(&device.mac_wlan0)),
            ("Build", field(&device.build_stamp)),
            ("License", "GPLv3".to_string()),
            ("Repo", REPO_URL.to_string()),
        ];

        let label_font = LoadedFont::new(FontFace::SansBold, 11);
        let value_font = LoadedFont::new(FontFace::MonoRegular, 12);
        let mut cy = HEADER_H + 6;
        for (label, value) in &rows {
            text(
                &mut canvas,
                &label_font,
                &label.to_ascii_uppercase(),
                LABEL_X,
                cy,
                palette.text_tertiary,
            );
            text(
                &mut canvas,
                &value_font,
                value,
                VALUE_X,
                cy - 1,
                palette.text_primary,
            );
            cy += ROW_H;
        }
        canvas
    }

    fn hit_zones(&self, _ctx: &PageContext) -> Vec<HitZone> {
        vec![detail_back_zone()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::palette::DARK;
    use crate::pages::{HitAction, PANEL_W};

    #[test]
    fn about_renders_with_back_zone() {
        let page = AboutDetailPage;
        let ctx = PageContext::default();
        let c = page.render(&ctx, &DARK);
        assert_eq!(c.width(), PANEL_W);
        let zones = page.hit_zones(&ctx);
        assert_eq!(zones.len(), 1);
        assert_eq!(zones[0].action, HitAction::Back);
    }

    #[test]
    fn about_renders_identity_rows_with_real_values() {
        let page = AboutDetailPage;
        let mut ctx = PageContext::default();
        ctx.device.version = Some("0.49.39".to_string());
        ctx.device.board_name = Some("Raspberry Pi 4B".to_string());
        ctx.device.device_id = Some("ados-58c27faf".to_string());
        ctx.device.device_name = Some("groundnode".to_string());
        ctx.device.mac_eth0 = Some("dc:a6:32:00:11:22".to_string());
        ctx.device.mac_wlan0 = Some("dc:a6:32:00:11:23".to_string());
        ctx.device.build_stamp = Some("2026-06-01".to_string());
        // A populated context paints body content, so the body region is no
        // longer entirely the background color.
        let c = page.render(&ctx, &DARK);
        let mut body_inked = false;
        for y in (HEADER_H + 1)..280 {
            for x in 0..PANEL_W as i32 {
                if c.pixel(x, y) != DARK.bg_primary {
                    body_inked = true;
                    break;
                }
            }
            if body_inked {
                break;
            }
        }
        assert!(body_inked);
    }

    #[test]
    fn missing_fields_render_dashes() {
        assert_eq!(field(&None), "--");
        assert_eq!(field(&Some(String::new())), "--");
        assert_eq!(field(&Some("0.1".to_string())), "0.1");
    }
}
