//! The early-life HARDWARE checklist and SETUP WIZARD tiles.

use super::text::{fit_font, truncate_to_width};
use super::HW_RADIO_ID;
use crate::graphics::fonts::{FontFace, LoadedFont};
use crate::graphics::palette::Palette;
use crate::graphics::primitives::{text, Canvas};
use crate::graphics::status_dot::draw_dot;
use crate::pages::{HardwareItem, PageContext};
use crate::widgets::draw_tile;

pub(super) fn draw_hardware_tile(
    canvas: &mut Canvas,
    palette: &Palette,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    hardware_check: &[HardwareItem],
) {
    let (bx, by, _bw, _bh) = draw_tile(canvas, palette, x, y, w, h, "Hardware", "checklist");

    // The rows the operator can act on at the bench, in priority order, keyed by
    // the setup service's hardware-check item ids. Whether a row is required
    // comes from the item itself (it depends on the node's profile).
    let rows: [(&str, &str); 4] = [
        ("board", "Companion compute"),
        (HW_RADIO_ID, "WFB radio adapter"),
        ("mesh_dongle", "Mesh second dongle"),
        ("display", "Local display"),
    ];

    let line_font = LoadedFont::new(FontFace::SansBold, 13);
    let detail_font = LoadedFont::new(FontFace::SansRegular, 11);

    let mut line_y = by + 4;
    for (item_id, label) in rows {
        let item = hardware_check
            .iter()
            .find(|it| it.id.as_deref() == Some(item_id));
        let required = item.is_some_and(|it| it.required);
        let state_val = item
            .and_then(|it| it.state.as_deref())
            .unwrap_or("unknown")
            .to_ascii_lowercase();
        let dot_color = if state_val == "ok" {
            palette.status_success
        } else if matches!(state_val.as_str(), "warning" | "missing") && required {
            palette.status_error
        } else if matches!(state_val.as_str(), "warning" | "missing") {
            palette.status_warning
        } else {
            palette.text_tertiary
        };

        draw_dot(canvas, bx + 6, line_y + 8, dot_color, 4, palette.bg_primary);
        text(
            canvas,
            &line_font,
            label,
            bx + 18,
            line_y + 1,
            palette.text_primary,
        );

        // Fix hint — only when required and missing — to nudge the operator
        // toward the physical action.
        let mut hint_text = String::new();
        if required && matches!(state_val.as_str(), "warning" | "missing") {
            let raw = item.and_then(|it| it.fix_hint.as_deref()).unwrap_or("");
            hint_text = if raw.chars().count() > 36 {
                let head: String = raw.chars().take(33).collect();
                format!("{head}…")
            } else {
                raw.to_string()
            };
        }
        if !hint_text.is_empty() {
            text(
                canvas,
                &detail_font,
                &hint_text,
                bx + 18,
                line_y + 16,
                palette.status_warning,
            );
            line_y += 30;
        } else {
            line_y += 22;
        }
    }
}

// ── early-life: SETUP WIZARD (replaces MESH) ────────────────────────

pub(super) fn draw_setup_wizard_tile(
    canvas: &mut Canvas,
    palette: &Palette,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    ctx: &PageContext,
) {
    let completion = ctx.completion_percent;
    let next_action = ctx.next_action.as_deref().unwrap_or("");

    // The agent's setup server redirects "/" to the wizard, so the bare host
    // URL is enough and saves horizontal pixels. With no reach name known the
    // tile says so instead of naming a host that may not resolve.
    let url = ctx.setup_url();

    let title_right = match completion {
        Some(c) => format!("{}%", c as i64),
        None => String::new(),
    };
    let (bx, by, bw, _bh) = draw_tile(canvas, palette, x, y, w, h, "Setup wizard", &title_right);

    // URL — biggest monospace size that fits the body width.
    match url.as_deref() {
        Some(url) => {
            let url_px = fit_font(FontFace::MonoBold, url, bw, 14, 10);
            let url_font = LoadedFont::new(FontFace::MonoBold, url_px);
            text(canvas, &url_font, url, bx, by + 4, palette.text_primary);
        }
        None => {
            let url_font = LoadedFont::new(FontFace::MonoBold, 14);
            text(canvas, &url_font, "—", bx, by + 4, palette.text_tertiary);
        }
    }

    // Next action — label plus a measure-and-truncate value.
    let action_font = LoadedFont::new(FontFace::SansBold, 12);
    let label = "Next:";
    let label_w = action_font.text_size(label).0 as i32;
    text(
        canvas,
        &action_font,
        label,
        bx,
        by + 26,
        palette.text_tertiary,
    );
    let next_avail = (bw - label_w - 6).max(0);
    let next_src = if next_action.is_empty() {
        "open the URL above"
    } else {
        next_action
    };
    let next_text = truncate_to_width(FontFace::SansBold, 12, next_src, next_avail);
    text(
        canvas,
        &action_font,
        &next_text,
        bx + label_w + 6,
        by + 26,
        palette.text_secondary,
    );

    // Tiny LAN hint, also safety-truncated.
    let hint_font = LoadedFont::new(FontFace::SansRegular, 10);
    let hint = truncate_to_width(
        FontFace::SansRegular,
        10,
        "from any browser on this LAN",
        bw,
    );
    text(
        canvas,
        &hint_font,
        &hint,
        bx,
        by + 50,
        palette.text_tertiary,
    );
}
