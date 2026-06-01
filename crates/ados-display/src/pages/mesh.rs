//! Mesh detail page.
//!
//! The drilldown from the dashboard's mesh tile. It shows the active role with
//! a switch-role button, one of three bodies (a "not a mesh node" notice for a
//! direct role, a "mesh down" checking notice with an orbiting progress dot, or
//! a scrollable peer list of up to six visible rows), and a footer carrying the
//! gateway short-id plus a partition badge.
//!
//! Tapping the switch-role button toggles a role-picker overlay over the bottom
//! half of the page; tapping a choice commits it. The role / mesh / peer data
//! all come from [`PageContext::role`] and [`PageContext::mesh`].

use std::cell::Cell;

use embedded_graphics::pixelcolor::Rgb888;

use crate::graphics::fonts::{FontFace, LoadedFont};
use crate::graphics::palette::Palette;
use crate::graphics::primitives::{fill_circle, fill_rect, fill_rect_outline, line, text, Canvas};
use crate::pages::{blank_panel, HitAction, HitZone, MeshCtx, Page, PageContext};
use crate::widgets::{draw_detail_header, DETAIL_HEADER_H};

/// Layout reference width / height of the detail-modal surface.
const PAGE_W: i32 = 480;
const PAGE_H: i32 = 244;
/// Header band height shared by every detail modal.
const HEADER_H: i32 = DETAIL_HEADER_H;

const ROLE_BADGE_DOT_R: i32 = 5;
const ROLE_ROW_Y: i32 = HEADER_H + 4;
const SWITCH_BTN_X: i32 = PAGE_W - 100;
const SWITCH_BTN_Y: i32 = ROLE_ROW_Y;
const SWITCH_BTN_W: i32 = 90;
const SWITCH_BTN_H: i32 = 24;
const PEER_LIST_Y: i32 = HEADER_H + 36;
const PEER_LIST_H: i32 = PAGE_H - PEER_LIST_Y - 28;
const PEER_ROW_H: i32 = 24;
const PEER_ROWS_VISIBLE: i32 = 6;
const FOOTER_Y: i32 = PAGE_H - 28;

/// The three selectable mesh roles, in picker display order.
const ROLE_CHOICES: [&str; 3] = ["direct", "relay", "receiver"];

/// Resolve a role string to its badge color.
fn role_color(palette: &Palette, role: &str) -> Rgb888 {
    match role.to_ascii_lowercase().as_str() {
        "direct" => palette.text_secondary,
        "relay" => palette.accent_primary,
        "receiver" => palette.status_success,
        _ => palette.text_tertiary,
    }
}

/// The mesh detail view, registered as `details.mesh`.
pub struct MeshDetailPage {
    /// Whether the role-picker overlay is open.
    picker_open: Cell<bool>,
    /// Peer-list scroll offset in whole rows.
    scroll_offset: Cell<i32>,
    /// Render-tick counter that orbits the "checking" progress dot.
    tick: Cell<u64>,
}

impl Default for MeshDetailPage {
    fn default() -> Self {
        Self {
            picker_open: Cell::new(false),
            scroll_offset: Cell::new(0),
            tick: Cell::new(0),
        }
    }
}

impl MeshDetailPage {
    /// Build a fresh mesh detail page.
    pub fn new() -> Self {
        Self::default()
    }

    /// Toggle the role-picker overlay open / closed.
    pub fn toggle_picker(&self) {
        self.picker_open.set(!self.picker_open.get());
    }

    /// Close the role-picker overlay (e.g. after a choice commits).
    pub fn close_picker(&self) {
        self.picker_open.set(false);
    }

    /// Whether the role-picker overlay is currently open.
    pub fn picker_open(&self) -> bool {
        self.picker_open.get()
    }

    /// Scroll the peer list by `rows` (positive scrolls down the list).
    pub fn scroll_by(&self, rows: i32) {
        let next = (self.scroll_offset.get() + rows).max(0);
        self.scroll_offset.set(next);
    }
}

impl Page for MeshDetailPage {
    fn id(&self) -> &'static str {
        "details.mesh"
    }

    fn refresh_hz(&self) -> f32 {
        2.0
    }

    fn render(&self, ctx: &PageContext, palette: &Palette) -> Canvas {
        self.tick.set(self.tick.get().wrapping_add(1));
        let mut canvas = blank_panel(palette);
        draw_detail_header(&mut canvas, palette, "Mesh");

        let role = ctx
            .role
            .current
            .clone()
            .unwrap_or_else(|| "direct".to_string())
            .to_ascii_lowercase();

        // Role badge row.
        let dot_color = role_color(palette, &role);
        let dot_cx = 12 + ROLE_BADGE_DOT_R;
        let dot_cy = ROLE_ROW_Y + 6 + ROLE_BADGE_DOT_R;
        fill_circle(
            &mut canvas,
            dot_cx,
            dot_cy,
            ROLE_BADGE_DOT_R,
            dot_color,
            None,
        );
        let role_font = LoadedFont::new(FontFace::SansBold, 14);
        text(
            &mut canvas,
            &role_font,
            &role.to_ascii_uppercase(),
            28,
            ROLE_ROW_Y + 4,
            palette.text_primary,
        );

        // Switch-role button.
        fill_rect_outline(
            &mut canvas,
            SWITCH_BTN_X,
            SWITCH_BTN_Y,
            SWITCH_BTN_X + SWITCH_BTN_W - 1,
            SWITCH_BTN_Y + SWITCH_BTN_H - 1,
            palette.bg_secondary,
            palette.border_strong,
        );
        let switch_label = "Switch role";
        let switch_font = LoadedFont::new(FontFace::SansBold, 11);
        let (sw, sh) = switch_font.text_size(switch_label);
        text(
            &mut canvas,
            &switch_font,
            switch_label,
            SWITCH_BTN_X + (SWITCH_BTN_W - sw as i32) / 2,
            SWITCH_BTN_Y + (SWITCH_BTN_H - sh as i32) / 2 - 1,
            palette.text_primary,
        );

        // Body selection: a direct role shows the not-a-mesh-node notice, a
        // mesh-capable role with the carrier down shows the checking notice,
        // otherwise the peer list.
        if role == "direct" {
            render_direct_body(&mut canvas, palette);
        } else if !ctx.mesh.up {
            render_mesh_down_body(&mut canvas, palette, self.tick.get());
        } else {
            render_peer_list(&mut canvas, palette, &ctx.mesh, self.scroll_offset.get());
        }

        // Footer: gateway + partition status.
        let gw = ctx
            .mesh
            .selected_gateway
            .clone()
            .unwrap_or_else(|| "--".to_string());
        let footer_font = LoadedFont::new(FontFace::MonoRegular, 11);
        text(
            &mut canvas,
            &footer_font,
            &format!("gw {gw}"),
            12,
            FOOTER_Y + 6,
            palette.text_secondary,
        );
        if ctx.mesh.partition {
            let label = "PARTITIONED";
            let label_font = LoadedFont::new(FontFace::SansBold, 11);
            let lw = label_font.text_size(label).0 as i32;
            fill_rect(
                &mut canvas,
                PAGE_W - lw - 16,
                FOOTER_Y + 4,
                PAGE_W - 8,
                FOOTER_Y + 22,
                palette.status_warning,
            );
            text(
                &mut canvas,
                &label_font,
                label,
                PAGE_W - lw - 12,
                FOOTER_Y + 6,
                palette.bg_primary,
            );
        }

        if self.picker_open.get() {
            render_picker(&mut canvas, palette, &role);
        }
        canvas
    }

    fn hit_zones(&self, _ctx: &PageContext) -> Vec<HitZone> {
        let mut zones = vec![
            HitZone::new(8, 8, 40, 32, HitAction::Back),
            HitZone::new(
                SWITCH_BTN_X,
                SWITCH_BTN_Y,
                SWITCH_BTN_W,
                SWITCH_BTN_H,
                HitAction::Custom("mesh.switch_role".to_string()),
            ),
        ];
        if self.picker_open.get() {
            let ovr_y = PEER_LIST_Y - 4;
            let row_h = (PAGE_H - 4 - ovr_y) / 3;
            for (i, choice) in ROLE_CHOICES.iter().enumerate() {
                zones.push(HitZone::new(
                    4,
                    ovr_y + i as i32 * row_h,
                    PAGE_W - 8,
                    row_h,
                    HitAction::Custom(format!("mesh.role.{choice}")),
                ));
            }
        } else {
            for i in 0..PEER_ROWS_VISIBLE {
                zones.push(HitZone::new(
                    8,
                    PEER_LIST_Y + i * PEER_ROW_H,
                    PAGE_W - 16,
                    PEER_ROW_H,
                    HitAction::Custom(format!("mesh.peer.{i}")),
                ));
            }
        }
        zones
    }
}

/// Paint the direct-role body: a "not a mesh node" notice centered in the list
/// region.
fn render_direct_body(canvas: &mut Canvas, palette: &Palette) {
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
fn render_mesh_down_body(canvas: &mut Canvas, palette: &Palette, tick: u64) {
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

/// Paint up to six visible peer rows starting at the scroll offset.
fn render_peer_list(canvas: &mut Canvas, palette: &Palette, mesh: &MeshCtx, scroll_offset: i32) {
    let peers = &mesh.peers;
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

/// Paint the bottom-half role-picker overlay with the three role choices.
fn render_picker(canvas: &mut Canvas, palette: &Palette, current: &str) {
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
        let label = choice.to_ascii_uppercase();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::palette::DARK;
    use crate::pages::{MeshPeer, PANEL_W};

    #[test]
    fn mesh_renders_with_peer_zones_when_closed() {
        let page = MeshDetailPage::new();
        let mut ctx = PageContext::default();
        ctx.role.current = Some("receiver".to_string());
        ctx.mesh.up = true;
        ctx.mesh.peers = (0..3)
            .map(|i| MeshPeer {
                device_id: Some(format!("ados-peer-{i:08x}")),
                role: Some("relay".to_string()),
                last_seen_seconds_ago: Some(i as f64 * 2.0),
            })
            .collect();
        let c = page.render(&ctx, &DARK);
        assert_eq!(c.width(), PANEL_W);
        let zones = page.hit_zones(&ctx);
        // Back + switch + six peer rows.
        assert_eq!(zones.len(), 2 + PEER_ROWS_VISIBLE as usize);
        assert_eq!(zones[0].action, HitAction::Back);
        assert_eq!(
            zones[1].action,
            HitAction::Custom("mesh.switch_role".to_string())
        );
    }

    #[test]
    fn picker_swaps_the_zone_set() {
        let page = MeshDetailPage::new();
        let ctx = PageContext::default();
        page.toggle_picker();
        assert!(page.picker_open());
        let c = page.render(&ctx, &DARK);
        assert_eq!(c.width(), PANEL_W);
        let zones = page.hit_zones(&ctx);
        // Back + switch + three role choices.
        assert_eq!(zones.len(), 5);
        assert_eq!(
            zones[2].action,
            HitAction::Custom("mesh.role.direct".to_string())
        );
        assert_eq!(
            zones[4].action,
            HitAction::Custom("mesh.role.receiver".to_string())
        );
        page.close_picker();
        assert!(!page.picker_open());
    }

    #[test]
    fn direct_role_renders_notice() {
        let page = MeshDetailPage::new();
        let mut ctx = PageContext::default();
        ctx.role.current = Some("direct".to_string());
        let c = page.render(&ctx, &DARK);
        assert_eq!(c.width(), PANEL_W);
    }

    #[test]
    fn scroll_never_goes_negative() {
        let page = MeshDetailPage::new();
        page.scroll_by(-5);
        assert_eq!(page.scroll_offset.get(), 0);
        page.scroll_by(3);
        assert_eq!(page.scroll_offset.get(), 3);
    }
}
