//! Settings page — scrollable list of configuration rows.
//!
//! The top-level Settings tab. It renders a vertical column of 48 px rows, each
//! binding a label, a current value (default rows), an on/off switch (toggle
//! rows), or an optional status string (action rows). A reboot banner sits above
//! the list whenever there are changes pending a service restart.
//!
//! The row data — id, label, variant, value, and toggle state — is resolved
//! upstream and gathered in [`PageContext::settings`]. This composer reads that
//! list and paints it; the reboot-banner count comes from
//! [`SettingsCtx::pending_reboot_count`].

use embedded_graphics::pixelcolor::Rgb888;

use crate::graphics::fonts::{FontFace, LoadedFont};
use crate::graphics::palette::Palette;
use crate::graphics::primitives::{fill_circle, fill_rect, fill_rect_outline, line, text, Canvas};
use crate::pages::{
    blank_panel, HitAction, HitZone, Page, PageContext, SettingsRow, CONTENT_H, CONTENT_Y, PANEL_W,
};
use crate::widgets::{bottom_bar_zones, draw_bottom_bar, draw_top_bar};

/// Height of one settings list row.
const ROW_H: i32 = 48;
/// Height of the reboot banner that sits above the list when changes are
/// pending a restart.
const BANNER_H: i32 = 28;

/// Pixel padding from the left edge for the label.
const LEFT_PAD: i32 = 12;
/// Pixel padding from the right edge for the chevron / switch / status.
const RIGHT_PAD: i32 = 12;

/// Toggle-switch geometry for the toggle variant.
const SWITCH_W: i32 = 36;
const SWITCH_H: i32 = 20;

/// The scrollable settings list, registered as `settings`.
pub struct SettingsPage;

impl Page for SettingsPage {
    fn id(&self) -> &'static str {
        "settings"
    }

    fn refresh_hz(&self) -> f32 {
        2.0
    }

    fn render(&self, ctx: &PageContext, palette: &Palette) -> Canvas {
        let mut canvas = blank_panel(palette);
        draw_top_bar(
            &mut canvas,
            palette,
            &ctx.hostname,
            ctx.role.current.as_deref().unwrap_or("unset"),
            ctx.system.cpu_pct,
            ctx.system.ram_used_mb,
            ctx.system.ram_total_mb,
            ctx.system.temp_c,
            &ctx.clock,
        );
        render_content(&mut canvas, palette, ctx);
        draw_bottom_bar(&mut canvas, palette, self.id());
        canvas
    }

    fn hit_zones(&self, ctx: &PageContext) -> Vec<HitZone> {
        let mut zones: Vec<HitZone> = Vec::new();
        let mut list_top = 0;
        let pending = ctx.settings.pending_reboot_count;
        if pending > 0 {
            zones.push(HitZone::new(
                0,
                0,
                PANEL_W as i32,
                BANNER_H,
                HitAction::Custom("banner.reboot".to_string()),
            ));
            list_top = BANNER_H;
        }
        let content_h = CONTENT_H as i32;
        for (i, row) in ctx.settings.rows.iter().enumerate() {
            let row_y = list_top + i as i32 * ROW_H;
            if row_y + ROW_H <= list_top || row_y >= content_h {
                continue;
            }
            let top = list_top.max(row_y);
            let bottom = content_h.min(row_y + ROW_H);
            zones.push(HitZone::new(
                0,
                top,
                PANEL_W as i32,
                bottom - top,
                HitAction::Custom(format!("row:{}", row.id)),
            ));
        }
        zones.extend(bottom_bar_zones());
        zones
    }
}

/// Paint the reboot banner (when pending) and the row list into the content
/// region. Page-local coordinates shift down by [`CONTENT_Y`] for the
/// panel-global paint.
fn render_content(canvas: &mut Canvas, palette: &Palette, ctx: &PageContext) {
    let oy = CONTENT_Y as i32;
    // Clear the content region so a redraw never keeps a previous page's pixels.
    fill_rect(
        canvas,
        0,
        oy,
        PANEL_W as i32 - 1,
        oy + CONTENT_H as i32 - 1,
        palette.bg_primary,
    );

    let mut list_top = 0;
    if ctx.settings.pending_reboot_count > 0 {
        draw_reboot_banner(canvas, palette, oy, ctx.settings.pending_reboot_count);
        list_top = BANNER_H;
    }

    let content_h = CONTENT_H as i32;
    for (i, row) in ctx.settings.rows.iter().enumerate() {
        let row_y = list_top + i as i32 * ROW_H;
        if row_y + ROW_H <= list_top {
            continue;
        }
        if row_y >= content_h {
            break;
        }
        draw_list_row(canvas, palette, oy + row_y, row);
    }

    // Top edge of the list where rows clip into the banner / status bar.
    line(
        canvas,
        0,
        oy + list_top,
        PANEL_W as i32 - 1,
        oy + list_top,
        palette.border_default,
    );
}

/// Paint a 28 px amber reboot banner with the pending-change copy and a
/// right-aligned "Reboot now" call-to-action.
fn draw_reboot_banner(canvas: &mut Canvas, palette: &Palette, oy: i32, count: i64) {
    fill_rect(
        canvas,
        0,
        oy,
        PANEL_W as i32 - 1,
        oy + BANNER_H - 1,
        palette.status_warning,
    );
    let body = if count <= 1 {
        "1 setting requires a reboot".to_string()
    } else {
        format!("{count} settings require a reboot")
    };
    let body_font = LoadedFont::new(FontFace::SansBold, 12);
    let (_, bh) = body_font.text_size(&body);
    text(
        canvas,
        &body_font,
        &body,
        12,
        oy + (BANNER_H - bh as i32) / 2 - 1,
        palette.bg_primary,
    );

    let cta = "Reboot now ›";
    let cta_font = LoadedFont::new(FontFace::SansBold, 12);
    let (cw, ch) = cta_font.text_size(cta);
    let cx = PANEL_W as i32 - 12 - cw as i32;
    let cy = oy + (BANNER_H - ch as i32) / 2 - 1;
    text(canvas, &cta_font, cta, cx, cy, palette.bg_primary);
}

/// Paint one 48 px list row at panel-global `y`. The variant selects the right
/// column: a value plus chevron (default), a switch (toggle), or an optional
/// muted status string (action). A 1 px divider closes the bottom edge so a
/// stack of rows reads as one list.
fn draw_list_row(canvas: &mut Canvas, palette: &Palette, y: i32, row: &SettingsRow) {
    let label_x = LEFT_PAD;

    // Label, vertically centered. Sans Regular 14, primary text.
    let label_font = LoadedFont::new(FontFace::SansRegular, 14);
    let (_, label_h) = label_font.text_size(&row.label);
    let label_y = y + (ROW_H - label_h as i32) / 2 - 2;
    text(
        canvas,
        &label_font,
        &row.label,
        label_x,
        label_y,
        palette.text_primary,
    );

    match row.variant.as_str() {
        "toggle" => draw_switch(canvas, palette, y, row.toggle_on.unwrap_or(false)),
        "action" => {
            if let Some(status) = action_status(row) {
                let status_font = LoadedFont::new(FontFace::MonoRegular, 11);
                let (sw, sh) = status_font.text_size(&status);
                let sx = PANEL_W as i32 - RIGHT_PAD - sw as i32;
                let sy = y + (ROW_H - sh as i32) / 2 - 1;
                text(canvas, &status_font, &status, sx, sy, palette.text_tertiary);
            }
        }
        _ => {
            // default: optional value text + chevron.
            let chevron_x = PANEL_W as i32 - RIGHT_PAD - 8;
            if let Some(value) = row.value.as_deref() {
                if !value.is_empty() {
                    let value_font = LoadedFont::new(FontFace::MonoRegular, 12);
                    let (vw, vh) = value_font.text_size(value);
                    let vx = chevron_x - 12 - vw as i32;
                    let vy = y + (ROW_H - vh as i32) / 2 - 1;
                    text(canvas, &value_font, value, vx, vy, palette.text_secondary);
                }
            }
            draw_chevron(canvas, chevron_x, y + ROW_H / 2, palette.text_tertiary);
        }
    }

    // Divider under the row.
    line(
        canvas,
        0,
        y + ROW_H - 1,
        PANEL_W as i32 - 1,
        y + ROW_H - 1,
        palette.border_default,
    );
}

/// Resolve an action row's optional status string. Action rows do not carry a
/// value column, but the snapshot may stash a short status (for example a
/// "running" hint) in the row's value field; surface it when present.
fn action_status(row: &SettingsRow) -> Option<String> {
    row.value
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(String::from)
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

/// Draw a 36x20 toggle switch right-aligned to the row at panel-global `y`.
fn draw_switch(canvas: &mut Canvas, palette: &Palette, y: i32, on: bool) {
    let sx = PANEL_W as i32 - RIGHT_PAD - SWITCH_W;
    let sy = y + (ROW_H - SWITCH_H) / 2;
    let bg = if on {
        palette.accent_primary
    } else {
        palette.bg_tertiary
    };
    let border = if on {
        palette.accent_primary
    } else {
        palette.border_strong
    };
    fill_rect_outline(
        canvas,
        sx,
        sy,
        sx + SWITCH_W - 1,
        sy + SWITCH_H - 1,
        bg,
        border,
    );
    // Knob: a white circle inset 2 px on the active side.
    let knob_d = SWITCH_H - 4;
    let knob_r = knob_d / 2;
    let knob_y = sy + 2 + knob_r;
    let knob_x = if on {
        sx + SWITCH_W - knob_d - 2 + knob_r
    } else {
        sx + 2 + knob_r
    };
    fill_circle(canvas, knob_x, knob_y, knob_r, palette.text_primary, None);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::palette::DARK;
    use crate::pages::SettingsCtx;

    fn sample_rows() -> Vec<SettingsRow> {
        vec![
            SettingsRow {
                id: "wfb.channel".to_string(),
                label: "Channel".to_string(),
                variant: "default".to_string(),
                value: Some("149".to_string()),
                toggle_on: None,
            },
            SettingsRow {
                id: "wfb.auto_pair".to_string(),
                label: "Auto-pair".to_string(),
                variant: "toggle".to_string(),
                value: None,
                toggle_on: Some(true),
            },
            SettingsRow {
                id: "system.reboot".to_string(),
                label: "Reboot now".to_string(),
                variant: "action".to_string(),
                value: None,
                toggle_on: None,
            },
        ]
    }

    fn ctx_with_settings(settings: SettingsCtx) -> PageContext {
        PageContext {
            settings,
            ..Default::default()
        }
    }

    #[test]
    fn settings_renders_rows_and_tab_zones() {
        let page = SettingsPage;
        let ctx = ctx_with_settings(SettingsCtx {
            rows: sample_rows(),
            ..Default::default()
        });
        let c = page.render(&ctx, &DARK);
        assert_eq!(c.width(), PANEL_W);
        // Three rows + five tab zones, no reboot banner.
        let zones = page.hit_zones(&ctx);
        assert_eq!(zones.len(), 8);
        assert_eq!(page.id(), "settings");
        assert!(zones
            .iter()
            .any(|z| z.action == HitAction::Custom("row:wfb.channel".to_string())));
    }

    #[test]
    fn reboot_banner_adds_a_zone_and_inks_amber() {
        let page = SettingsPage;
        let ctx = ctx_with_settings(SettingsCtx {
            rows: sample_rows(),
            pending_reboot_count: 2,
            ..Default::default()
        });
        let zones = page.hit_zones(&ctx);
        // Banner zone + three rows + five tabs.
        assert_eq!(zones.len(), 9);
        assert_eq!(
            zones[0].action,
            HitAction::Custom("banner.reboot".to_string())
        );
        let c = page.render(&ctx, &DARK);
        // The banner band paints the warning color at the top of the content.
        assert_eq!(c.pixel(0, CONTENT_Y as i32), DARK.status_warning);
    }

    #[test]
    fn toggle_on_inks_the_accent() {
        let page = SettingsPage;
        let ctx = ctx_with_settings(SettingsCtx {
            rows: vec![SettingsRow {
                id: "wfb.auto_pair".to_string(),
                label: "Auto-pair".to_string(),
                variant: "toggle".to_string(),
                value: None,
                toggle_on: Some(true),
            }],
            ..Default::default()
        });
        let c = page.render(&ctx, &DARK);
        let oy = CONTENT_Y as i32;
        let mut found = false;
        for y in oy..(oy + ROW_H) {
            for x in (PANEL_W as i32 - RIGHT_PAD - SWITCH_W)..(PANEL_W as i32 - RIGHT_PAD) {
                if c.pixel(x, y) == DARK.accent_primary {
                    found = true;
                }
            }
        }
        assert!(found, "an on toggle should paint the accent color");
    }

    #[test]
    fn empty_settings_still_carries_tab_zones() {
        let page = SettingsPage;
        let ctx = PageContext::default();
        let zones = page.hit_zones(&ctx);
        assert_eq!(zones.len(), 5);
    }
}
