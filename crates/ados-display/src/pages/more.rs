//! More page — overflow menu of secondary actions.
//!
//! A short list of operator actions that drill into detail pages or run an
//! agent action: pair drone, diagnostics, restart agent, about. Restart takes
//! two taps: the first arms it (the row reads "Tap again to restart") for
//! [`RESTART_CONFIRM_WINDOW`], the second sends
//! `POST /api/v1/system/restart-supervisor`. Each
//! row is a 48 px list row in the shared list styling — a left-aligned label
//! with a right-pointing chevron — and the four rows fit inside the content
//! area without a scroll envelope (`4 * 48 = 192` px).

use std::cell::Cell;
use std::time::{Duration, Instant};

use embedded_graphics::pixelcolor::Rgb888;

use crate::graphics::fonts::{FontFace, LoadedFont};
use crate::graphics::palette::Palette;
use crate::graphics::primitives::{fill_rect, line, text, Canvas};
use crate::pages::{
    blank_panel, AgentRequest, Chrome, HitAction, HitZone, Page, PageContext, PanelAction, PANEL_W,
};

/// Height of one overflow list row.
pub const ROW_H: i32 = 48;

/// Pixel padding from the left edge for the label.
const LEFT_PAD: i32 = 12;
/// Pixel padding from the right edge for the chevron.
const RIGHT_PAD: i32 = 12;

/// How long a first tap on "Restart agent" stays armed for the confirming tap.
pub const RESTART_CONFIRM_WINDOW: Duration = Duration::from_secs(5);

/// The restart row's custom key.
const RESTART_KEY: &str = "more.restart";

/// The overflow menu, registered as `more`.
#[derive(Default)]
pub struct MorePage {
    /// When the restart row was armed by a first tap.
    restart_armed_at: Cell<Option<Instant>>,
}

impl MorePage {
    fn restart_armed(&self) -> bool {
        self.restart_armed_at
            .get()
            .is_some_and(|t| t.elapsed() < RESTART_CONFIRM_WINDOW)
    }

    /// The rows, in display order: `(zone key, operator label, drill-into page
    /// id or action)`. A `None` target marks an agent-action row resolved by
    /// [`Page::on_custom`].
    const ROWS: [(&'static str, &'static str, Option<&'static str>); 4] = [
        ("more.pair", "Pair drone", Some("details.pair_drone")),
        (
            "more.diagnostics",
            "Diagnostics",
            Some("details.diagnostics"),
        ),
        (RESTART_KEY, "Restart agent", None),
        ("more.about", "About", Some("details.about")),
    ];
}

impl Page for MorePage {
    fn id(&self) -> &'static str {
        "more"
    }

    fn chrome(&self) -> Chrome {
        Chrome::FullScreen
    }

    fn refresh_hz(&self) -> f32 {
        2.0
    }

    fn render(&self, _ctx: &PageContext, palette: &Palette) -> Canvas {
        let mut canvas = blank_panel(palette);
        for (i, (key, label, _target)) in Self::ROWS.iter().enumerate() {
            let row_y = i as i32 * ROW_H;
            let label = if *key == RESTART_KEY && self.restart_armed() {
                "Tap again to restart"
            } else {
                label
            };
            draw_list_row(&mut canvas, palette, row_y, label);
        }
        canvas
    }

    fn hit_zones(&self, _ctx: &PageContext) -> Vec<HitZone> {
        Self::ROWS
            .iter()
            .enumerate()
            .map(|(i, (key, _label, target))| {
                let action = match target {
                    Some(page_id) => HitAction::OpenDetail(page_id),
                    None => HitAction::Custom((*key).to_string()),
                };
                HitZone::new(0, i as i32 * ROW_H, PANEL_W as i32, ROW_H, action)
            })
            .collect()
    }

    fn on_custom(&self, key: &str, _ctx: &PageContext) -> Option<PanelAction> {
        if key != RESTART_KEY {
            return None;
        }
        if !self.restart_armed() {
            self.restart_armed_at.set(Some(Instant::now()));
            return Some(PanelAction::Repaint);
        }
        self.restart_armed_at.set(None);
        Some(PanelAction::Agent(AgentRequest {
            method: "POST",
            path: "/api/v1/system/restart-supervisor",
            body: None,
            label: "Restart agent".to_string(),
        }))
    }
}

/// Paint one 48 px overflow row at `y`: a left-aligned label in primary text, a
/// right-pointing chevron, and a 1 px divider closing the bottom edge so the
/// stack reads as one list.
fn draw_list_row(canvas: &mut Canvas, palette: &Palette, y: i32, label: &str) {
    // Row plate so a redraw never keeps a previous surface behind the row.
    fill_rect(
        canvas,
        0,
        y,
        PANEL_W as i32 - 1,
        y + ROW_H - 1,
        palette.bg_primary,
    );

    let label_font = LoadedFont::new(FontFace::SansRegular, 14);
    let (_, label_h) = label_font.text_size(label);
    let label_y = y + (ROW_H - label_h as i32) / 2 - 2;
    text(
        canvas,
        &label_font,
        label,
        LEFT_PAD,
        label_y,
        palette.text_primary,
    );

    let chevron_x = PANEL_W as i32 - RIGHT_PAD - 8;
    draw_chevron(canvas, chevron_x, y + ROW_H / 2, palette.text_tertiary);

    line(
        canvas,
        0,
        y + ROW_H - 1,
        PANEL_W as i32 - 1,
        y + ROW_H - 1,
        palette.border_default,
    );
}

/// Draw a right-pointing chevron centered on `(cx, cy)`.
fn draw_chevron(canvas: &mut Canvas, cx: i32, cy: i32, color: Rgb888) {
    let arm = 5;
    // Two-pixel stroke, one row apart, so the chevron reads on the panel.
    for off in 0..2 {
        line(canvas, cx - arm, cy - arm + off, cx, cy + off, color);
        line(canvas, cx, cy + off, cx - arm, cy + arm + off, color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::palette::DARK;

    #[test]
    fn more_has_four_row_zones() {
        let page = MorePage::default();
        let ctx = PageContext::default();
        let c = page.render(&ctx, &DARK);
        assert_eq!(c.width(), PANEL_W);
        let zones = page.hit_zones(&ctx);
        assert_eq!(zones.len(), 4);
        assert_eq!(zones[0].action, HitAction::OpenDetail("details.pair_drone"));
        assert_eq!(
            zones[2].action,
            HitAction::Custom("more.restart".to_string())
        );
        assert_eq!(zones[3].action, HitAction::OpenDetail("details.about"));
    }

    #[test]
    fn each_row_paints_a_chevron_and_divider() {
        let page = MorePage::default();
        let c = page.render(&PageContext::default(), &DARK);
        // A divider line sits at the bottom edge of every row.
        for i in 0..4 {
            let y = i * ROW_H + ROW_H - 1;
            assert_eq!(c.pixel(0, y), DARK.border_default);
        }
        // The label is inked somewhere on the first row (not pure background).
        let mut inked = false;
        for y in 0..ROW_H {
            for x in LEFT_PAD..(LEFT_PAD + 120) {
                if c.pixel(x, y) != DARK.bg_primary && c.pixel(x, y) != DARK.border_default {
                    inked = true;
                }
            }
        }
        assert!(inked, "the row label should ink at least one pixel");
    }

    /// Restart needs a confirming second tap before it reaches the agent.
    #[test]
    fn restart_takes_two_taps() {
        let page = MorePage::default();
        let ctx = PageContext::default();
        assert_eq!(
            page.on_custom(RESTART_KEY, &ctx),
            Some(PanelAction::Repaint)
        );
        let Some(PanelAction::Agent(req)) = page.on_custom(RESTART_KEY, &ctx) else {
            panic!("the confirming tap must reach the agent");
        };
        assert_eq!(
            (req.method, req.path),
            ("POST", "/api/v1/system/restart-supervisor")
        );
        // Sent once; the next tap arms again rather than restarting again.
        assert_eq!(
            page.on_custom(RESTART_KEY, &ctx),
            Some(PanelAction::Repaint)
        );
    }
}
