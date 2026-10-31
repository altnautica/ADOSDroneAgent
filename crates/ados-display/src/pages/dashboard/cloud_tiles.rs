//! The UPLINK / CLOUD tile and its early-life PAIR DRONE variant.

use embedded_graphics::pixelcolor::Rgb888;

use super::text::{fit_font, truncate_to_width};
use crate::graphics::fonts::{FontFace, LoadedFont};
use crate::graphics::palette::Palette;
use crate::graphics::primitives::{text, Canvas};
use crate::graphics::qr::render_qr;
use crate::graphics::status_dot::draw_dot;
use crate::pages::{NetworkCtx, PageContext};
use crate::widgets::draw_tile;

#[allow(clippy::too_many_arguments)]
pub(super) fn draw_uplink_tile(
    canvas: &mut Canvas,
    palette: &Palette,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    network: &NetworkCtx,
    paired: bool,
    latency_ms: Option<f64>,
    pair_code: &str,
) {
    let title_right = match latency_ms {
        Some(ms) => format!("{} ms", ms as i64),
        None => String::new(),
    };
    let (bx, by, bw, _bh) = draw_tile(canvas, palette, x, y, w, h, "Uplink / Cloud", &title_right);

    // Uplink status row.
    let (dot_color, uplink_label) = uplink_status(palette, network);
    draw_dot(canvas, bx + 7, by + 14, dot_color, 6, palette.bg_primary);
    let label_font = LoadedFont::new(FontFace::SansBold, 14);
    text(
        canvas,
        &label_font,
        &uplink_label,
        bx + 22,
        by + 6,
        palette.text_primary,
    );

    // Mission Control pair status.
    let mc_font = LoadedFont::new(FontFace::SansRegular, 12);
    text(
        canvas,
        &mc_font,
        "Mission Control",
        bx,
        by + 38,
        palette.text_secondary,
    );

    if paired {
        let ok_font = LoadedFont::new(FontFace::SansBold, 13);
        text(
            canvas,
            &ok_font,
            "✓ paired",
            bx + 110,
            by + 38,
            palette.status_success,
        );
    } else if !pair_code.is_empty() {
        // Big pair code, centered, so it reads from across the bench.
        let code_font = LoadedFont::new(FontFace::MonoBold, 22);
        let cw = code_font.text_size(pair_code).0 as i32;
        text(
            canvas,
            &code_font,
            pair_code,
            bx + (bw - cw) / 2,
            by + 56,
            palette.text_primary,
        );
    } else {
        text(
            canvas,
            &mc_font,
            "—",
            bx + 110,
            by + 38,
            palette.text_tertiary,
        );
    }
}

/// The uplink row's dot colour and label. An uplink the agent does not report
/// is unknown (`—`, tertiary), never OFFLINE: only a reported `none` is.
pub(super) fn uplink_status(palette: &Palette, network: &NetworkCtx) -> (Rgb888, String) {
    let Some(kind) = network.uplink_type.as_deref().map(str::to_ascii_lowercase) else {
        return (palette.text_tertiary, "—".to_string());
    };
    if kind == "none" {
        return (palette.status_error, "OFFLINE".to_string());
    }
    let color = match network.uplink_reachable {
        Some(true) => palette.status_success,
        Some(false) => palette.status_warning,
        None => palette.text_tertiary,
    };
    (color, kind)
}

// ── early-life: PAIR DRONE (replaces DRONE) ─────────────────────────

#[allow(clippy::too_many_arguments)]
pub(super) fn draw_pair_drone_tile(
    canvas: &mut Canvas,
    palette: &Palette,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    ctx: &PageContext,
) {
    let code = ctx.pair_code().unwrap_or("").to_ascii_uppercase();

    let (bx, by, bw, bh) = draw_tile(canvas, palette, x, y, w, h, "Pair drone", "broadcast");

    // Broadcasting pulse — small dot in the title-bar slot. The pulse color
    // toggles when the agent is actively beaconing the code.
    let pulse_color = if ctx.cloud.broadcasting {
        palette.status_success
    } else {
        Rgb888::new(0x0E, 0x4D, 0x26)
    };
    draw_dot(
        canvas,
        x + w - 70,
        y + 10,
        pulse_color,
        3,
        palette.bg_primary,
    );

    // Layout: QR sized so the right column fits the big code plus two hint
    // lines. Cap so a readable text column remains. No code, no QR: there is
    // nothing for a phone to claim yet.
    let qr_size = (bh - 8).clamp(0, 78);
    let qr = if code.is_empty() {
        None
    } else {
        render_qr(&ctx.pair_deep_link(&code), qr_size as u32, 2)
    };
    let text_x = if let Some(qr) = qr.as_ref() {
        // The matrix is dark-on-light by QR convention; the prior renderer
        // pasted dark modules as the bright foreground over the dark ground,
        // so paint dark modules in primary text and skip light modules.
        for py in 0..qr.size {
            for px in 0..qr.size {
                if qr.is_dark(px, py) {
                    canvas.put_pixel(bx + px as i32, by + 4 + py as i32, palette.text_primary);
                }
            }
        }
        bx + qr_size + 10
    } else {
        bx
    };
    let text_w = (bw - (text_x - bx)).max(0);

    // Big pair code — auto-shrink so a six-char code fits the column.
    if !code.is_empty() {
        let code_px = fit_font(FontFace::MonoBold, &code, text_w, 26, 18);
        let code_font = LoadedFont::new(FontFace::MonoBold, code_px);
        text(
            canvas,
            &code_font,
            &code,
            text_x,
            by + 4,
            palette.text_primary,
        );
    } else {
        let loading_font = LoadedFont::new(FontFace::SansBold, 13);
        text(
            canvas,
            &loading_font,
            "waiting…",
            text_x,
            by + 14,
            palette.text_tertiary,
        );
    }

    // Hint lines — measure-and-truncate so a long string never bleeds.
    let hint_face = FontFace::SansRegular;
    let hint_font = LoadedFont::new(hint_face, 11);
    let line1 = truncate_to_width(hint_face, 11, "Open Mission Control →", text_w);
    let line2 = truncate_to_width(hint_face, 11, "Tap \"Pair drone\"", text_w);
    let line3 = truncate_to_width(hint_face, 11, "Enter code above", text_w);
    text(
        canvas,
        &hint_font,
        &line1,
        text_x,
        by + 38,
        palette.text_secondary,
    );
    text(
        canvas,
        &hint_font,
        &line2,
        text_x,
        by + 52,
        palette.text_secondary,
    );
    text(
        canvas,
        &hint_font,
        &line3,
        text_x,
        by + 66,
        palette.text_secondary,
    );
}
