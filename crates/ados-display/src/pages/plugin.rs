//! Reserved data-driven page for plugin-contributed content.
//!
//! The page registry keys compiled-in pages on a `&'static str` id, so a plugin
//! cannot add a page with a runtime id. Instead the display reserves ONE page
//! with the static id `"plugin"` whose content is data-driven: a plugin writes
//! `/run/ados/lcd-plugin-page.json` (a title, label/value rows, and optional
//! touch zones) and this page reads it each render. A plugin can present a
//! status surface (sensor readouts, device state, and similar) without
//! recompiling the display service.
//!
//! Unlike the chrome pages that compose from [`PageContext`], the plugin content
//! is not part of that shared context (no compiled-in page reaches outside its
//! own data), so this page reads the sidecar directly in `render` and
//! `hit_zones`. The page holds its sidecar path so tests inject a temp file
//! rather than touching `/run`.
//!
//! Each declared zone maps to a [`HitAction::Custom`] keyed by the zone's
//! `key`; the navigator hands that key back to the display service, which
//! forwards it to the owning plugin. When the sidecar is absent the page paints
//! a neutral placeholder and exposes no zones.

use std::path::PathBuf;

use crate::graphics::fonts::{FontFace, LoadedFont};
use crate::graphics::palette::Palette;
use crate::graphics::primitives::{fill_rect_outline, text, Canvas};
use crate::pages::{blank_panel, HitAction, HitZone, Page, PageContext};
use crate::sidecar::{LcdPluginPage, LCD_PLUGIN_PAGE_PATH};
use crate::widgets::{detail_back_zone, draw_detail_header, DETAIL_HEADER_H};

/// The stable route id every navigator keys this page on.
pub const PLUGIN_PAGE_ID: &str = "plugin";

/// Header band height shared by every detail-style page.
const HEADER_H: i32 = DETAIL_HEADER_H;
/// Vertical step between label/value rows.
const ROW_H: i32 = 22;
/// Left edge of the row labels.
const LABEL_X: i32 = 16;
/// Left edge of the row values.
const VALUE_X: i32 = 160;
/// First row baseline below the header band.
const FIRST_ROW_Y: i32 = HEADER_H + 8;
/// Title shown on the header band when the sidecar carries no title.
const DEFAULT_TITLE: &str = "Plugin";

/// The reserved plugin page. Holds the sidecar path it reads its content from.
pub struct PluginPage {
    sidecar_path: PathBuf,
}

impl PluginPage {
    /// Build the page reading the canonical `/run/ados/lcd-plugin-page.json`.
    pub fn new() -> Self {
        Self {
            sidecar_path: PathBuf::from(LCD_PLUGIN_PAGE_PATH),
        }
    }

    /// Build the page reading an explicit sidecar path (tests inject a temp
    /// file so the read round-trips without touching `/run`).
    pub fn with_sidecar_path(sidecar_path: PathBuf) -> Self {
        Self { sidecar_path }
    }

    /// Load the current plugin content, or `None` when no plugin has written a
    /// page (or the file is malformed).
    fn content(&self) -> Option<LcdPluginPage> {
        LcdPluginPage::load(&self.sidecar_path)
    }
}

impl Default for PluginPage {
    fn default() -> Self {
        Self::new()
    }
}

impl Page for PluginPage {
    fn id(&self) -> &'static str {
        PLUGIN_PAGE_ID
    }

    fn refresh_hz(&self) -> f32 {
        1.0
    }

    fn render(&self, _ctx: &PageContext, palette: &Palette) -> Canvas {
        let mut canvas = blank_panel(palette);
        let page = self.content();

        let title = page
            .as_ref()
            .map(|p| p.title.as_str())
            .filter(|t| !t.is_empty())
            .unwrap_or(DEFAULT_TITLE);
        draw_detail_header(&mut canvas, palette, title);

        let Some(page) = page else {
            // No plugin has contributed a page: neutral placeholder, no rows.
            let font = LoadedFont::new(FontFace::SansRegular, 12);
            text(
                &mut canvas,
                &font,
                "No plugin page",
                LABEL_X,
                HEADER_H + 12,
                palette.text_secondary,
            );
            return canvas;
        };

        // Label/value rows: caps label left, value right, clipped to the
        // content region so a long list cannot spill into the chrome.
        let label_font = LoadedFont::new(FontFace::SansBold, 11);
        let value_font = LoadedFont::new(FontFace::MonoRegular, 12);
        let mut cy = FIRST_ROW_Y;
        for row in &page.rows {
            if cy + ROW_H > crate::pages::PANEL_H as i32 {
                break;
            }
            text(
                &mut canvas,
                &label_font,
                &row.label.to_ascii_uppercase(),
                LABEL_X,
                cy,
                palette.text_tertiary,
            );
            text(
                &mut canvas,
                &value_font,
                &row.value,
                VALUE_X,
                cy - 1,
                palette.text_primary,
            );
            cy += ROW_H;
        }

        // Outline each declared zone with its caption so a touch target is
        // visible. Zones are page-local; offset into panel-global y by the top
        // chrome height for the outline only (hit-testing stays page-local).
        let zone_font = LoadedFont::new(FontFace::SansBold, 11);
        for zone in &page.zones {
            if zone.w <= 0 || zone.h <= 0 {
                continue;
            }
            let gx = zone.x;
            let gy = zone.y + crate::pages::TOP_BAR_H as i32;
            fill_rect_outline(
                &mut canvas,
                gx,
                gy,
                gx + zone.w - 1,
                gy + zone.h - 1,
                palette.bg_secondary,
                palette.border_default,
            );
            if !zone.label.is_empty() {
                text(
                    &mut canvas,
                    &zone_font,
                    &zone.label,
                    gx + 6,
                    gy + (zone.h - 11) / 2,
                    palette.text_primary,
                );
            }
        }

        canvas
    }

    fn hit_zones(&self, _ctx: &PageContext) -> Vec<HitZone> {
        // Always offer the back chip so the operator can leave the page even
        // when no plugin content is present.
        let mut zones = vec![detail_back_zone()];
        if let Some(page) = self.content() {
            for zone in &page.zones {
                if zone.w <= 0 || zone.h <= 0 || zone.key.is_empty() {
                    continue;
                }
                zones.push(HitZone::new(
                    zone.x,
                    zone.y,
                    zone.w,
                    zone.h,
                    HitAction::Custom(zone.key.clone()),
                ));
            }
        }
        zones
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::palette::DARK;
    use crate::pages::PANEL_W;
    use crate::sidecar::{LcdPluginRow, LcdPluginZone};

    fn write_page(dir: &std::path::Path, page: &LcdPluginPage) -> PathBuf {
        let path = dir.join("lcd-plugin-page.json");
        page.write_to(&path).unwrap();
        path
    }

    #[test]
    fn id_is_the_reserved_static_id() {
        assert_eq!(PluginPage::new().id(), "plugin");
    }

    #[test]
    fn absent_sidecar_renders_placeholder_and_only_back_zone() {
        let dir = tempfile::tempdir().unwrap();
        let page = PluginPage::with_sidecar_path(dir.path().join("absent.json"));
        let c = page.render(&PageContext::default(), &DARK);
        assert_eq!(c.width(), PANEL_W);
        let zones = page.hit_zones(&PageContext::default());
        assert_eq!(zones.len(), 1);
        assert_eq!(zones[0].action, HitAction::Back);
    }

    #[test]
    fn content_renders_rows_and_maps_zones_to_custom() {
        let dir = tempfile::tempdir().unwrap();
        let content = LcdPluginPage {
            title: "Sensor".to_string(),
            rows: vec![LcdPluginRow {
                label: "temp".to_string(),
                value: "42 C".to_string(),
            }],
            zones: vec![
                LcdPluginZone {
                    x: 8,
                    y: 40,
                    w: 120,
                    h: 32,
                    key: "tare".to_string(),
                    label: "Tare".to_string(),
                },
                // A zero-area zone is dropped from the hit zones.
                LcdPluginZone {
                    x: 0,
                    y: 0,
                    w: 0,
                    h: 0,
                    key: "ignored".to_string(),
                    label: String::new(),
                },
                // A keyless zone is dropped too.
                LcdPluginZone {
                    x: 200,
                    y: 40,
                    w: 50,
                    h: 32,
                    key: String::new(),
                    label: "x".to_string(),
                },
            ],
        };
        let path = write_page(dir.path(), &content);
        let page = PluginPage::with_sidecar_path(path);

        // The body inks (rows + zone outline painted over the background).
        let c = page.render(&PageContext::default(), &DARK);
        let mut body_inked = false;
        for y in (HEADER_H + 1)..276 {
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

        // Back zone + exactly the one valid custom zone.
        let zones = page.hit_zones(&PageContext::default());
        assert_eq!(zones.len(), 2);
        assert_eq!(zones[0].action, HitAction::Back);
        assert_eq!(zones[1].action, HitAction::Custom("tare".to_string()));
        assert_eq!(
            (zones[1].x, zones[1].y, zones[1].w, zones[1].h),
            (8, 40, 120, 32)
        );
    }

    #[test]
    fn malformed_sidecar_falls_back_to_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lcd-plugin-page.json");
        std::fs::write(&path, "not json").unwrap();
        let page = PluginPage::with_sidecar_path(path);
        let c = page.render(&PageContext::default(), &DARK);
        assert_eq!(c.width(), PANEL_W);
        assert_eq!(page.hit_zones(&PageContext::default()).len(), 1);
    }
}
