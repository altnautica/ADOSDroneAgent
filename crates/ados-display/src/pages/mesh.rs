//! Mesh detail page.
//!
//! The drilldown from the dashboard's mesh tile. It shows the active role with
//! a switch-role button, one of three bodies (a "not a mesh node" notice for a
//! direct role, a "mesh down" checking notice with an orbiting progress dot, or
//! a scrollable peer list of up to six visible rows), and a footer carrying the
//! gateway short-id plus a partition badge.
//!
//! Tapping the switch-role button toggles a role-picker overlay over the bottom
//! half of the page. A role change re-forms the link, so a choice takes two
//! taps: the first arms it (the row reads "Tap again: RELAY"), the second
//! closes the picker and writes the role through
//! `PUT /api/v1/ground-station/role`. The peer rows are a read-out, not
//! controls. The role / mesh / peer data all come from [`PageContext::role`] and
//! [`PageContext::mesh`].

use std::cell::Cell;

use embedded_graphics::pixelcolor::Rgb888;

use crate::graphics::fonts::{FontFace, LoadedFont};
use crate::graphics::palette::Palette;
use crate::graphics::primitives::{fill_circle, fill_rect, fill_rect_outline, text, Canvas};
use crate::pages::{
    blank_panel, AgentRequest, Chrome, HitAction, HitZone, MeshState, Page, PageContext,
    PanelAction, TwoTapConfirm,
};
use crate::widgets::{draw_detail_header, DETAIL_HEADER_H};

mod render;

use render::{
    render_direct_body, render_mesh_down_body, render_notice_body, render_peer_list, render_picker,
};

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
    /// Two-tap arm state of the role choices, keyed by role.
    role_confirm: TwoTapConfirm,
}

impl Default for MeshDetailPage {
    fn default() -> Self {
        Self {
            picker_open: Cell::new(false),
            scroll_offset: Cell::new(0),
            tick: Cell::new(0),
            role_confirm: TwoTapConfirm::default(),
        }
    }
}

impl MeshDetailPage {
    /// Build a fresh mesh detail page.
    pub fn new() -> Self {
        Self::default()
    }

    /// Toggle the role-picker overlay open / closed. Any armed choice is
    /// dropped, so reopening never inherits a half-confirmed role.
    pub fn toggle_picker(&self) {
        self.picker_open.set(!self.picker_open.get());
        self.role_confirm.disarm();
    }

    /// Close the role-picker overlay (e.g. after a choice commits).
    pub fn close_picker(&self) {
        self.picker_open.set(false);
        self.role_confirm.disarm();
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

    fn chrome(&self) -> Chrome {
        Chrome::FullScreen
    }

    fn refresh_hz(&self) -> f32 {
        2.0
    }

    fn render(&self, ctx: &PageContext, palette: &Palette) -> Canvas {
        self.tick.set(self.tick.get().wrapping_add(1));
        let mut canvas = blank_panel(palette);
        draw_detail_header(&mut canvas, palette, "Mesh");

        // A missing role is unknown, not direct: a failed status poll must not
        // present this node as "not a mesh node".
        let role = ctx
            .role
            .current
            .as_deref()
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();

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
        let role_label = if role.is_empty() {
            "ROLE UNKNOWN".to_string()
        } else {
            role.to_ascii_uppercase()
        };
        text(
            &mut canvas,
            &role_font,
            &role_label,
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

        // Body selection: a direct role shows the not-a-mesh-node notice; an
        // unknown role or mesh state says so; a mesh that reports down shows the
        // checking notice; otherwise the peer list.
        if role == "direct" {
            render_direct_body(&mut canvas, palette);
        } else if role.is_empty() {
            render_notice_body(
                &mut canvas,
                palette,
                "Role unknown",
                "no status from the agent",
            );
        } else {
            match ctx.mesh.state() {
                MeshState::Unknown => render_notice_body(
                    &mut canvas,
                    palette,
                    "Mesh state unknown",
                    "no current mesh snapshot",
                ),
                MeshState::Down => render_mesh_down_body(&mut canvas, palette, self.tick.get()),
                MeshState::Up => {
                    render_peer_list(&mut canvas, palette, &ctx.mesh, self.scroll_offset.get())
                }
            }
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
            let armed = ROLE_CHOICES
                .iter()
                .copied()
                .find(|r| self.role_confirm.is_armed(r));
            render_picker(&mut canvas, palette, &role, armed);
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
        }
        zones
    }

    fn on_custom(&self, key: &str, _ctx: &PageContext) -> Option<PanelAction> {
        if key == "mesh.switch_role" {
            self.toggle_picker();
            return Some(PanelAction::Repaint);
        }
        let role = ROLE_CHOICES
            .iter()
            .copied()
            .find(|r| key.strip_prefix("mesh.role.") == Some(*r))?;
        if !self.role_confirm.tap(role) {
            return Some(PanelAction::Repaint);
        }
        self.close_picker();
        Some(PanelAction::Agent(AgentRequest {
            method: "PUT",
            path: "/api/v1/ground-station/role",
            body: Some(serde_json::json!({ "role": role })),
            label: format!("Mesh role {role}"),
        }))
    }
}

#[cfg(test)]
mod tests;
