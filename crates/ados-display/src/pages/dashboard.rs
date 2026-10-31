//! Dashboard page — the 4-tile live status grid.
//!
//! The top-level Dashboard tab. It paints the persistent top status bar over a
//! 480x244 content region (the navigator paints the tab bar below it) that holds a 2x2 grid of
//! status tiles, with early-life variants swapping in when a slot's primary
//! data is missing. Each tile is a tap target that drills into the matching
//! detail page.
//!
//! Grid order (top-left to bottom-right):
//!
//! ```text
//!   RADIO LINK   |   DRONE
//!   -------------+-----------
//!   MESH         |   UPLINK / CLOUD
//! ```
//!
//! A slot routes to its early-life variant when the primary data is absent:
//! RADIO LINK becomes HARDWARE (no radio adapter), DRONE becomes PAIR DRONE
//! (no drone phoned home), MESH becomes SETUP WIZARD (wizard not finalized or
//! role unset). UPLINK / CLOUD is always relevant and never swaps out.
//!
//! Tile geometry mirrors the inset content region: an 8 px outer margin and an
//! 8 px inter-tile gap, so each tile is `(480 - 16 - 8) / 2 = 228` wide by
//! `(244 - 16 - 8) / 2 = 110` tall. The tiles render in page-local content
//! coordinates and are offset by [`CONTENT_Y`] when painted onto the full panel.

use crate::graphics::palette::Palette;
use crate::graphics::primitives::{fill_rect, Canvas};
use crate::pages::{
    blank_panel, tile_rects, Chrome, HitAction, HitZone, Page, PageContext, CONTENT_H, CONTENT_Y,
    PANEL_W,
};
use crate::widgets::draw_top_bar;

mod cloud_tiles;
mod fleet_tiles;
mod link_tile;
mod setup_tiles;
mod text;

use cloud_tiles::{draw_pair_drone_tile, draw_uplink_tile};
use fleet_tiles::{draw_drone_tile, draw_mesh_tile};
use link_tile::draw_radio_link_tile;
use setup_tiles::{draw_hardware_tile, draw_setup_wizard_tile};

/// Cap used for the bitrate meter fill. WFB-ng on the reference RTL8812EU rigs
/// tops out near here, so the segmented meter reads full at this throughput.
const BITRATE_CAP_MBPS: f64 = 35.0;

/// TX-power threshold past which host-VBUS topology starts to brown out the
/// radio on the reference single-board buses; the brownout pill escalates the
/// TX line once power is pushed past this and the supply path is host-VBUS.
const BROWNOUT_TX_DBM_THRESHOLD: i64 = 12;

/// The setup service's hardware-check id for the WFB radio adapter
/// (`HardwareCheckItem(id="radio_wfb")`).
const HW_RADIO_ID: &str = "radio_wfb";

/// The live 4-tile dashboard, registered as `dashboard`.
pub struct DashboardPage;

impl Page for DashboardPage {
    fn id(&self) -> &'static str {
        "dashboard"
    }

    fn chrome(&self) -> Chrome {
        Chrome::Tabbed
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
        render_inset(&mut canvas, palette, ctx);
        canvas
    }

    fn hit_zones(&self, ctx: &PageContext) -> Vec<HitZone> {
        // The zones follow the same routing the paint uses, so a tap always opens
        // the page behind the tile actually on screen.
        tile_rects()
            .iter()
            .zip(route_tiles(ctx))
            .filter_map(|(t, slot)| {
                slot.detail()
                    .map(|detail| HitZone::new(t.x, t.y, t.w, t.h, HitAction::OpenDetail(detail)))
            })
            .collect()
    }
}

/// Which renderer fills each grid slot for a given context. The default tiles
/// take over once their data is live; the early-life variants take the slot
/// while their primary data is missing so no tile wastes pixels on dashes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    RadioLink,
    Hardware,
    Drone,
    PairDrone,
    Mesh,
    SetupWizard,
    Uplink,
}

impl Slot {
    /// The detail page a tap on this tile opens, or `None` for a tile with
    /// nothing to drill into.
    fn detail(self) -> Option<&'static str> {
        match self {
            Slot::RadioLink => Some("details.radio_link"),
            // The checklist's drill-in is the board and system view.
            Slot::Hardware => Some("details.diagnostics"),
            Slot::Drone => Some("details.drone"),
            Slot::PairDrone => Some("details.pair_drone"),
            Slot::Mesh => Some("details.mesh"),
            // The wizard runs in a browser at the URL the tile shows.
            Slot::SetupWizard => None,
            Slot::Uplink => Some("details.uplink"),
        }
    }
}

/// Pick the four tile renderers for the current context.
///
/// LINK slot swaps to HARDWARE only on a clear early-life signal: no RSSI and
/// no bitrate, and the hardware check flags the radio adapter missing or
/// warning. DRONE swaps to PAIR DRONE when nothing about a drone is known.
/// MESH swaps to SETUP WIZARD while the wizard is unfinalized or the role is
/// unset. UPLINK / CLOUD is always shown.
fn route_tiles(ctx: &PageContext) -> [Slot; 4] {
    let link = &ctx.link;
    let drone = &ctx.drone;
    let role = &ctx.role;
    let cloud = &ctx.cloud;

    let no_link_signal = link.rssi_dbm.is_none() && link.bitrate_kbps.is_none();
    let radio_missing_in_hw_check = ctx
        .hardware_check
        .iter()
        .find(|it| it.id.as_deref() == Some(HW_RADIO_ID))
        .map(|it| {
            matches!(
                it.state.as_deref().map(str::to_ascii_lowercase).as_deref(),
                Some("missing") | Some("warning")
            )
        })
        .unwrap_or(false);
    let top_left = if no_link_signal && radio_missing_in_hw_check {
        Slot::Hardware
    } else {
        Slot::RadioLink
    };

    let drone_unpaired = drone.device_id.is_none()
        && drone.battery_pct.is_none()
        && drone.gps_sats.is_none()
        && !cloud.paired;
    let top_right = if drone_unpaired {
        Slot::PairDrone
    } else {
        Slot::Drone
    };

    let role_lower = role.current.as_deref().unwrap_or("").to_ascii_lowercase();
    let wizard_pending = !ctx.setup_finalized || role_lower.is_empty() || role_lower == "unset";
    let bottom_left = if wizard_pending {
        Slot::SetupWizard
    } else {
        Slot::Mesh
    };

    [top_left, top_right, bottom_left, Slot::Uplink]
}

/// Paint the 4-tile grid into the chrome-less inset content region.
///
/// The page navigator owns the top status bar and bottom tab bar; this paints
/// the four tiles into the 480x244 region between them. Tile coordinates from
/// [`tile_rects`] are page-local, so each tile is offset down by [`CONTENT_Y`]
/// before painting onto the full panel.
fn render_inset(canvas: &mut Canvas, palette: &Palette, ctx: &PageContext) {
    // Clear the content region to the background so a redraw never keeps a
    // previous page's pixels behind the tiles.
    fill_rect(
        canvas,
        0,
        CONTENT_Y as i32,
        PANEL_W as i32 - 1,
        (CONTENT_Y + CONTENT_H) as i32 - 1,
        palette.bg_primary,
    );

    let tiles = tile_rects();
    let slots = route_tiles(ctx);
    for (tile, slot) in tiles.iter().zip(slots) {
        // Tile geometry is page-local; shift to panel-global for the paint.
        let x = tile.x;
        let y = tile.y + CONTENT_Y as i32;
        let w = tile.w;
        let h = tile.h;
        match slot {
            Slot::RadioLink => draw_radio_link_tile(canvas, palette, x, y, w, h, ctx),
            Slot::Hardware => draw_hardware_tile(canvas, palette, x, y, w, h, &ctx.hardware_check),
            Slot::Drone => draw_drone_tile(
                canvas,
                palette,
                x,
                y,
                w,
                h,
                &ctx.drone,
                ctx.pair_code().unwrap_or(""),
            ),
            Slot::PairDrone => draw_pair_drone_tile(canvas, palette, x, y, w, h, ctx),
            Slot::Mesh => draw_mesh_tile(canvas, palette, x, y, w, h, &ctx.role, &ctx.mesh),
            Slot::SetupWizard => draw_setup_wizard_tile(canvas, palette, x, y, w, h, ctx),
            Slot::Uplink => draw_uplink_tile(
                canvas,
                palette,
                x,
                y,
                w,
                h,
                &ctx.network,
                ctx.cloud.paired,
                ctx.cloud.latency_ms,
                ctx.pair_code().unwrap_or(""),
            ),
        }
    }
}

#[cfg(test)]
mod tests;
