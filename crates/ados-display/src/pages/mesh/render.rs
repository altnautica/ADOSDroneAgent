//! Body painters for the mesh detail page: notices, the peer list and the
//! role-picker overlay.

use super::{
    role_color, PAGE_H, PAGE_W, PEER_LIST_H, PEER_LIST_Y, PEER_ROWS_VISIBLE, PEER_ROW_H,
    ROLE_CHOICES,
};
use crate::graphics::fonts::{FontFace, LoadedFont};
use crate::graphics::palette::Palette;
use crate::graphics::primitives::{fill_circle, fill_rect, fill_rect_outline, line, text, Canvas};
use crate::pages::MeshCtx;

/// Paint a two-line tertiary notice centered in the list region.
pub(super) fn render_notice_body(canvas: &mut Canvas, palette: &Palette, msg: &str, sub: &str) {
    let font = LoadedFont::new(FontFace::SansBold, 14);
    let (mw, mh) = font.text_size(msg);
    text(
        canvas,
        &font,
        msg,
        (PAGE_W - mw as i32) / 2,
        PEER_LIST_Y + (PEER_LIST_H - mh as i32) / 2 - 6,
        palette.text_secondary,
    );
    let sub_font = LoadedFont::new(FontFace::SansRegular, 11);
    let sw = sub_font.text_size(sub).0 as i32;
    text(
        canvas,
        &sub_font,
        sub,
        (PAGE_W - sw) / 2,
        PEER_LIST_Y + (PEER_LIST_H - mh as i32) / 2 + 14,
        palette.text_tertiary,
    );
}

/// Paint the direct-role body: a "not a mesh node" notice centered in the list
/// region.
pub(super) fn render_direct_body(canvas: &mut Canvas, palette: &Palette) {
    let msg = "Not a mesh node";
    let font = LoadedFont::new(FontFace::SansBold, 14);
    let (mw, mh) = font.text_size(msg);
    text(
        canvas,
        &font,
        msg,
        (PAGE_W - mw as i32) / 2,
        PEER_LIST_Y + (PEER_LIST_H - mh as i32) / 2 - 6,
        palette.text_secondary,
    );
    let sub = "this node is in direct role";
    let sub_font = LoadedFont::new(FontFace::SansRegular, 11);
    let sw = sub_font.text_size(sub).0 as i32;
    text(
        canvas,
        &sub_font,
        sub,
        (PAGE_W - sw) / 2,
        PEER_LIST_Y + (PEER_LIST_H - mh as i32) / 2 + 14,
        palette.text_tertiary,
    );
}

/// Paint the mesh-down body: a warning notice plus an orbiting progress dot.
pub(super) fn render_mesh_down_body(canvas: &mut Canvas, palette: &Palette, tick: u64) {
    let msg = "Mesh down -- checking";
    let font = LoadedFont::new(FontFace::SansBold, 14);
    let (mw, mh) = font.text_size(msg);
    text(
        canvas,
        &font,
        msg,
        (PAGE_W - mw as i32) / 2,
        PEER_LIST_Y + (PEER_LIST_H - mh as i32) / 2 - 4,
        palette.status_warning,
    );
    // Orbit a small dot to make progress visible without an animation framework.
    let cx = PAGE_W / 2;
    let cy = PEER_LIST_Y + (PEER_LIST_H - mh as i32) / 2 + 22;
    let angle = (tick % 8) as f64 * (std::f64::consts::PI / 4.0);
    let radius = 6.0;
    let dx = (angle.cos() * radius).round() as i32;
    let dy = (angle.sin() * radius).round() as i32;
    fill_circle(canvas, cx + dx, cy + dy, 3, palette.accent_primary, None);
}

/// Paint up to six visible peer rows starting at the scroll offset. With no
/// roster in the snapshot, the peer count is all that is known and is shown as
/// such, rather than an empty list that reads as "nobody visible".
pub(super) fn render_peer_list(
    canvas: &mut Canvas,
    palette: &Palette,
    mesh: &MeshCtx,
    scroll_offset: i32,
) {
    let Some(peers) = mesh.peers.as_deref() else {
        let count = match mesh.peer_count {
            Some(n) => format!("{n} peers"),
            None => "— peers".to_string(),
        };
        render_notice_body(canvas, palette, &count, "peer roster not reported");
        return;
    };
    let max_offset = (peers.len() as i32 - PEER_ROWS_VISIBLE).max(0);
    let offset = scroll_offset.clamp(0, max_offset);
    let start = offset as usize;
    let end = (start + PEER_ROWS_VISIBLE as usize).min(peers.len());
    let visible = &peers[start..end];

    let row_font = LoadedFont::new(FontFace::MonoRegular, 10);
    let badge_font = LoadedFont::new(FontFace::SansBold, 10);
    let seen_font = LoadedFont::new(FontFace::MonoRegular, 10);

    for (i, peer) in visible.iter().enumerate() {
        let row_y = PEER_LIST_Y + i as i32 * PEER_ROW_H;
        line(
            canvas,
            8,
            row_y + PEER_ROW_H - 1,
            PAGE_W - 8,
            row_y + PEER_ROW_H - 1,
            palette.border_default,
        );
        let dev = peer.device_id.clone().unwrap_or_else(|| "--".to_string());
        // Last 12 chars of the device id.
        let short: String = {
            let chars: Vec<char> = dev.chars().collect();
            let from = chars.len().saturating_sub(12);
            chars[from..].iter().collect()
        };
        text(
            canvas,
            &row_font,
            &short,
            12,
            row_y + 6,
            palette.text_primary,
        );

        let role = peer.role.clone().unwrap_or_default().to_ascii_lowercase();
        let badge_color = role_color(palette, &role);
        let badge = if role.is_empty() {
            "--".to_string()
        } else {
            role.to_ascii_uppercase()
        };
        text(canvas, &badge_font, &badge, 220, row_y + 6, badge_color);

        let seen_text = match peer.last_seen_seconds_ago {
            Some(s) => format!("{}s", s as i64),
            None => "--".to_string(),
        };
        let sw = seen_font.text_size(&seen_text).0 as i32;
        text(
            canvas,
            &seen_font,
            &seen_text,
            PAGE_W - sw - 12,
            row_y + 6,
            palette.text_tertiary,
        );
    }
    if visible.is_empty() {
        let empty_font = LoadedFont::new(FontFace::SansRegular, 11);
        let msg = "no peers visible";
        let mw = empty_font.text_size(msg).0 as i32;
        text(
            canvas,
            &empty_font,
            msg,
            (PAGE_W - mw) / 2,
            PEER_LIST_Y + 30,
            palette.text_tertiary,
        );
    }
}

/// Paint the bottom-half role-picker overlay with the three role choices. The
/// `armed` choice reads "Tap again: ROLE" until its confirming tap.
pub(super) fn render_picker(
    canvas: &mut Canvas,
    palette: &Palette,
    current: &str,
    armed: Option<&str>,
) {
    let ovr_y = PEER_LIST_Y - 4;
    fill_rect_outline(
        canvas,
        4,
        ovr_y,
        PAGE_W - 4,
        PAGE_H - 4,
        palette.bg_secondary,
        palette.border_strong,
    );
    let row_h = (PAGE_H - 4 - ovr_y) / ROLE_CHOICES.len() as i32;
    let row_font = LoadedFont::new(FontFace::SansBold, 14);
    for (i, choice) in ROLE_CHOICES.iter().enumerate() {
        let ry = ovr_y + i as i32 * row_h;
        let is_current = *choice == current;
        if is_current {
            fill_rect(
                canvas,
                4,
                ry,
                PAGE_W - 4,
                ry + row_h - 1,
                palette.bg_tertiary,
            );
        }
        let label = if armed == Some(*choice) {
            format!("Tap again: {}", choice.to_ascii_uppercase())
        } else {
            choice.to_ascii_uppercase()
        };
        let (lw, lh) = row_font.text_size(&label);
        let color = if is_current {
            palette.accent_primary
        } else {
            palette.text_primary
        };
        text(
            canvas,
            &row_font,
            &label,
            (PAGE_W - lw as i32) / 2,
            ry + (row_h - lh as i32) / 2 - 1,
            color,
        );
    }
}
