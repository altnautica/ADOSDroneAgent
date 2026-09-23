//! Dashboard page — the 4-tile live status grid.
//!
//! The top-level Dashboard tab. It paints the persistent top status bar and
//! bottom tab bar around a 480x244 content region that holds a 2x2 grid of
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

use embedded_graphics::pixelcolor::Rgb888;

use crate::graphics::bar_meter::draw_bar;
use crate::graphics::fonts::{FontFace, LoadedFont};
use crate::graphics::palette::Palette;
use crate::graphics::primitives::{fill_rect, text, Canvas};
use crate::graphics::qr::render_qr;
use crate::graphics::status_dot::draw_dot;
use crate::graphics::thresholds;
use crate::pages::{
    blank_panel, tile_rects, ArmState, Chrome, DroneCtx, HardwareItem, HitAction, HitZone, LinkCtx,
    MeshCtx, MeshState, NetworkCtx, Page, PageContext, PairingCtx, RoleCtx, CONTENT_H, CONTENT_Y,
    PANEL_W,
};
use crate::widgets::{bottom_bar_zones, draw_big_number, draw_bottom_bar, draw_tile, draw_top_bar};

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
        draw_bottom_bar(&mut canvas, palette, self.id());
        canvas
    }

    fn hit_zones(&self, ctx: &PageContext) -> Vec<HitZone> {
        // The zones follow the same routing the paint uses, so a tap always opens
        // the page behind the tile actually on screen.
        let mut zones: Vec<HitZone> = tile_rects()
            .iter()
            .zip(route_tiles(ctx))
            .filter_map(|(t, slot)| {
                slot.detail()
                    .map(|detail| HitZone::new(t.x, t.y, t.w, t.h, HitAction::OpenDetail(detail)))
            })
            .collect();
        zones.extend(bottom_bar_zones());
        zones
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

    let no_link_signal = link.rssi_dbm.is_none() && link.bitrate_mbps.is_none();
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
            Slot::Drone => draw_drone_tile(canvas, palette, x, y, w, h, &ctx.drone, &ctx.pairing),
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
                pair_code(ctx),
            ),
        }
    }
}

/// The pair code any pairing surface shows: the local pairing window's code,
/// else the cloud one. Empty when neither is known.
fn pair_code(ctx: &PageContext) -> &str {
    ctx.pairing
        .code
        .as_deref()
        .filter(|s| !s.is_empty())
        .or(ctx.cloud.pair_code.as_deref())
        .unwrap_or("")
}

/// Whether the radio tile warns of a brownout: TX past the safe envelope on a
/// radio whose declared supply path is host VBUS. An unknown supply path never
/// warns; the warning is a statement about the topology, so it needs one.
fn brownout_risk(link: &LinkCtx) -> bool {
    link.topology.as_deref() == Some("host_vbus")
        && link
            .tx_power_dbm
            .is_some_and(|p| p > BROWNOUT_TX_DBM_THRESHOLD)
}

// ── small format + measure helpers ──────────────────────────────────

/// Group an integer with thousands separators (`1247` -> `1,247`), matching the
/// FEC counters' comma-grouped display.
fn group_thousands(value: i64) -> String {
    let neg = value < 0;
    let digits = value.unsigned_abs().to_string();
    let mut out = String::new();
    let len = digits.len();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    if neg {
        format!("-{out}")
    } else {
        out
    }
}

/// Inked width of `s` at `(face, px)`.
fn measure(face: FontFace, px: u32, s: &str) -> i32 {
    LoadedFont::new(face, px).text_size(s).0 as i32
}

/// Largest size in `[min_px, max_px]` whose inked width of `s` fits `max_w`,
/// floored at `min_px`. Mirrors the auto-shrink the pair-code and wizard-URL
/// tiles use so a long string never overflows its column.
fn fit_font(face: FontFace, s: &str, max_w: i32, max_px: u32, min_px: u32) -> u32 {
    let mut px = max_px;
    while px > min_px {
        if measure(face, px, s) <= max_w {
            return px;
        }
        px -= 1;
    }
    min_px
}

/// Trim `s` with a trailing ellipsis until it fits `max_w` at `(face, px)`.
fn truncate_to_width(face: FontFace, px: u32, s: &str, max_w: i32) -> String {
    if measure(face, px, s) <= max_w {
        return s.to_string();
    }
    let mut chars: Vec<char> = s.chars().collect();
    while !chars.is_empty() {
        chars.pop();
        let candidate: String = chars.iter().collect::<String>() + "…";
        if measure(face, px, &candidate) <= max_w {
            return candidate;
        }
    }
    String::new()
}

// ── Tile A — RADIO LINK ─────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn draw_radio_link_tile(
    canvas: &mut Canvas,
    palette: &Palette,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    ctx: &PageContext,
) {
    let link: &LinkCtx = &ctx.link;
    let rssi = link.rssi_dbm;
    let bitrate = link.bitrate_mbps;
    let fec_rec = link.fec_recovered;
    let fec_lost = link.fec_lost;
    let channel = link.channel;
    let tx_power_dbm = link.tx_power_dbm;

    let title_right = match channel {
        Some(c) => format!("ch {c}"),
        None => String::new(),
    };
    let (bx, by, bw, bh) = draw_tile(canvas, palette, x, y, w, h, "Radio link", &title_right);

    // Topology chip in the title-bar top-right, placed just left of where the
    // channel caption starts (or flush to the right edge when there is none).
    // No chip when the radio has not declared its supply path.
    if let Some(topology) = link.topology.as_deref() {
        let mut chip_anchor_x = x + w - 8;
        if !title_right.is_empty() {
            let right_text_w = measure(FontFace::MonoRegular, 11, &title_right);
            chip_anchor_x = x + w - 8 - right_text_w - 4;
        }
        draw_topology_badge(canvas, palette, chip_anchor_x, y + 3, topology);
    }

    // A stale snapshot carries no readings (the context nulls them); say why the
    // tile is empty rather than leaving a blank that reads as a dead radio.
    if link.is_stale() {
        let chip_font = LoadedFont::new(FontFace::SansBold, 10);
        let label = "STALE";
        let (cw, ch) = chip_font.text_size(label);
        let cx0 = bx + bw - cw as i32 - 6;
        fill_rect(
            canvas,
            cx0 - 3,
            by + 4,
            cx0 + cw as i32 + 2,
            by + 4 + ch as i32 + 3,
            palette.status_warning,
        );
        text(canvas, &chip_font, label, cx0, by + 5, palette.bg_primary);
    }

    // Big RSSI value with threshold color.
    let (rssi_text, rssi_color, rssi_unit) = match rssi {
        None => ("— dBm".to_string(), palette.text_tertiary, ""),
        Some(v) => (
            format!("{}", v.round() as i64),
            palette.grade(Some(v), thresholds::RSSI_DBM),
            "dBm",
        ),
    };
    draw_big_number(
        canvas,
        bx,
        by + 2,
        &rssi_text,
        rssi_color,
        30,
        rssi_unit,
        palette.text_secondary,
    );

    // TX power line — dim secondary metadata between the headline and bitrate.
    let tx_y = by + 36;
    let tx_text = match tx_power_dbm {
        None => "TX -- dBm".to_string(),
        Some(v) => format!("TX {v} dBm"),
    };
    let tx_font = LoadedFont::new(FontFace::MonoRegular, 11);
    text(canvas, &tx_font, &tx_text, bx, tx_y, palette.text_secondary);

    // Bitrate row — value plus a segmented bar.
    let bitrate_y = by + 52;
    match bitrate {
        None => {
            let f = LoadedFont::new(FontFace::MonoRegular, 14);
            text(canvas, &f, "— Mbps", bx, bitrate_y, palette.text_tertiary);
        }
        Some(v) => {
            let f = LoadedFont::new(FontFace::MonoBold, 14);
            let s = format!("{} Mbps", v.round() as i64);
            text(canvas, &f, &s, bx, bitrate_y, palette.text_primary);
            let bar_x = bx + 90;
            let bar_w = (bw - 90).max(0) as u32;
            draw_bar(
                canvas,
                bar_x,
                bitrate_y + 4,
                bar_w,
                8,
                Some(v / BITRATE_CAP_MBPS),
                6,
                palette.status_success,
                palette.border_strong,
                2,
            );
        }
    }

    // FEC counters.
    let fec_y = by + 72;
    let rec_str = fec_rec
        .map(group_thousands)
        .unwrap_or_else(|| "—".to_string());
    let lost_str = fec_lost
        .map(group_thousands)
        .unwrap_or_else(|| "—".to_string());
    let lost_color = if fec_lost.unwrap_or(0) > 100 {
        palette.status_error
    } else {
        palette.text_secondary
    };
    let fec_font = LoadedFont::new(FontFace::MonoRegular, 12);
    text(canvas, &fec_font, "FEC R", bx, fec_y, palette.text_tertiary);
    text(
        canvas,
        &fec_font,
        &rec_str,
        bx + 38,
        fec_y,
        palette.text_secondary,
    );
    text(
        canvas,
        &fec_font,
        "L",
        bx + 100,
        fec_y,
        palette.text_tertiary,
    );
    text(canvas, &fec_font, &lost_str, bx + 114, fec_y, lost_color);

    // Brownout warning pill — only when on host-VBUS and TX is past the safe
    // envelope. Plain ASCII label for reliable rendering on the panel pipeline.
    if brownout_risk(link) {
        let label = "BROWNOUT RISK";
        let pill_font = LoadedFont::new(FontFace::SansBold, 10);
        let (pill_text_w, pill_text_h) = pill_font.text_size(label);
        let pill_h = pill_text_h as i32 + 4;
        let pill_y0 = by + bh - pill_h;
        fill_rect(
            canvas,
            bx,
            pill_y0,
            bx + bw - 1,
            pill_y0 + pill_h - 1,
            palette.status_warning,
        );
        text(
            canvas,
            &pill_font,
            label,
            bx + (bw - pill_text_w as i32) / 2,
            pill_y0 + 1,
            palette.bg_primary,
        );
    }
}

/// Topology badge palette for the radio-link chip: the four-char chip signals
/// the radio's power-supply path. An unrecognised value shows as itself.
fn topology_badge<'a>(palette: &Palette, topology: &'a str) -> (&'a str, Rgb888) {
    match topology {
        "powered_hub" => ("HUB", palette.accent_primary),
        "external_5v" => ("EXT", palette.status_success),
        "host_vbus" => ("VBUS", palette.border_strong),
        other => (other, palette.border_strong),
    }
}

/// Paint the four-char topology chip with its right edge at `x` and top at `y`.
fn draw_topology_badge(canvas: &mut Canvas, palette: &Palette, x: i32, y: i32, topology: &str) {
    let (label, fill) = topology_badge(palette, topology);
    let badge_font = LoadedFont::new(FontFace::SansBold, 9);
    let (text_w, text_h) = badge_font.text_size(label);
    let (pad_x, pad_y) = (4, 1);
    let chip_w = text_w as i32 + pad_x * 2;
    let chip_h = text_h as i32 + pad_y * 2;
    let chip_x0 = x - chip_w;
    let chip_y0 = y;
    fill_rect(
        canvas,
        chip_x0,
        chip_y0,
        chip_x0 + chip_w - 1,
        chip_y0 + chip_h - 1,
        fill,
    );
    text(
        canvas,
        &badge_font,
        label,
        chip_x0 + pad_x,
        chip_y0 + pad_y - 1,
        palette.text_primary,
    );
}

// ── Tile B — DRONE ──────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn draw_drone_tile(
    canvas: &mut Canvas,
    palette: &Palette,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    drone: &DroneCtx,
    pairing: &PairingCtx,
) {
    let device_id = drone.device_id.as_deref();
    let fc_mode = drone.fc_mode.as_deref();
    let battery = drone.battery_pct;
    let gps_sats = drone.gps_sats;

    let title_right = match device_id {
        Some(id) if !id.is_empty() => last_n_upper(id, 6),
        _ => String::new(),
    };
    let (bx, by, bw, bh) = draw_tile(canvas, palette, x, y, w, h, "Drone", &title_right);

    // Empty state when no drone is paired / sending heartbeats.
    let no_device = device_id.map(str::is_empty).unwrap_or(true);
    if no_device && battery.is_none() && fc_mode.is_none() && gps_sats.is_none() {
        let empty_font = LoadedFont::new(FontFace::SansBold, 14);
        let msg = "NO DRONE PAIRED";
        let (ew, eh) = empty_font.text_size(msg);
        text(
            canvas,
            &empty_font,
            msg,
            bx + (bw - ew as i32) / 2,
            by + (bh - eh as i32) / 2 - 6,
            palette.text_secondary,
        );
        let code = pairing.code.as_deref().unwrap_or("");
        if !code.is_empty() {
            let code_str = format!("pair: {code}");
            let code_font = LoadedFont::new(FontFace::MonoBold, 12);
            let cw = code_font.text_size(&code_str).0 as i32;
            text(
                canvas,
                &code_font,
                &code_str,
                bx + (bw - cw) / 2,
                by + (bh - eh as i32) / 2 + 14,
                palette.text_tertiary,
            );
        }
        return;
    }

    // Mode + arm row.
    // Arm state only as reported: with no report it reads `ARM —`, never
    // DISARMED, since an unreported vehicle may be armed and flying.
    let arm = ArmState::from_report(drone.armed);
    let arm_font = LoadedFont::new(FontFace::SansBold, 14);
    text(
        canvas,
        &arm_font,
        arm.label(),
        bx,
        by + 4,
        arm.color(palette),
    );
    if let Some(mode) = fc_mode {
        if !mode.is_empty() {
            let mode_font = LoadedFont::new(FontFace::MonoBold, 16);
            let mode_text = first_n_upper(mode, 6);
            let mw = mode_font.text_size(&mode_text).0 as i32;
            text(
                canvas,
                &mode_font,
                &mode_text,
                bx + bw - mw,
                by + 2,
                palette.text_primary,
            );
        }
    }

    // Battery headline + GPS sat count.
    if let Some(b) = battery {
        let bat_color = palette.grade(Some(b), thresholds::BATTERY_PCT);
        draw_big_number(
            canvas,
            bx,
            by + 32,
            &format!("{}", b as i64),
            bat_color,
            28,
            "%",
            palette.text_secondary,
        );
    } else {
        let f = LoadedFont::new(FontFace::MonoRegular, 14);
        text(canvas, &f, "BAT —", bx, by + 38, palette.text_tertiary);
    }

    if let Some(sats) = gps_sats {
        let sat_font = LoadedFont::new(FontFace::MonoBold, 14);
        let (marker, sat_color) = if sats >= 6 {
            ("✓", palette.status_success)
        } else {
            ("⚠", palette.status_warning)
        };
        let sat_text = format!("GPS {sats} {marker}");
        let sw = sat_font.text_size(&sat_text).0 as i32;
        text(
            canvas,
            &sat_font,
            &sat_text,
            bx + bw - sw,
            by + 50,
            sat_color,
        );
    }
}

// ── Tile C — MESH ───────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn draw_mesh_tile(
    canvas: &mut Canvas,
    palette: &Palette,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    role_block: &RoleCtx,
    mesh_block: &MeshCtx,
) {
    let role = role_block
        .current
        .as_deref()
        .unwrap_or("")
        .to_ascii_lowercase();
    let mesh_capable = role_block.mesh_capable;

    let title_right = role.clone();
    let (bx, by, bw, bh) = draw_tile(canvas, palette, x, y, w, h, "Mesh", &title_right);

    // A direct-role node with no mesh capability shows N/A rather than dashes.
    if role == "direct" && !mesh_capable {
        let msg = "MESH N/A";
        let msg_font = LoadedFont::new(FontFace::SansBold, 14);
        let (mw, mh) = msg_font.text_size(msg);
        text(
            canvas,
            &msg_font,
            msg,
            bx + (bw - mw as i32) / 2,
            by + (bh - mh as i32) / 2 - 4,
            palette.text_tertiary,
        );
        let sub = "this node is in direct role";
        let sub_font = LoadedFont::new(FontFace::SansRegular, 11);
        let sw = sub_font.text_size(sub).0 as i32;
        text(
            canvas,
            &sub_font,
            sub,
            bx + (bw - sw) / 2,
            by + (bh - mh as i32) / 2 + 14,
            palette.text_tertiary,
        );
        return;
    }

    let partition = mesh_block.partition;
    let peers = match mesh_block.peer_count {
        Some(n) => format!("{n} peers"),
        None => "— peers".to_string(),
    };
    let selected_gateway = mesh_block.selected_gateway.as_deref();
    let mesh_id = mesh_block.mesh_id.as_deref().unwrap_or("");

    let (dot_color, status_label) = match mesh_block.state() {
        MeshState::Unknown => (palette.text_tertiary, "state unknown".to_string()),
        MeshState::Down => (palette.status_error, "down".to_string()),
        MeshState::Up if partition => (palette.status_warning, format!("partitioned · {peers}")),
        MeshState::Up => (palette.status_success, format!("up · {peers}")),
    };

    // Status row: dot + label.
    draw_dot(canvas, bx + 7, by + 14, dot_color, 6, palette.bg_primary);
    let status_font = LoadedFont::new(FontFace::SansBold, 14);
    text(
        canvas,
        &status_font,
        &status_label,
        bx + 22,
        by + 6,
        palette.text_primary,
    );

    // Gateway row.
    let detail_font = LoadedFont::new(FontFace::SansRegular, 12);
    match selected_gateway {
        Some(gw) if !gw.is_empty() => {
            text(
                canvas,
                &detail_font,
                &format!("gw: {gw}"),
                bx,
                by + 38,
                palette.text_secondary,
            );
        }
        _ => {
            text(
                canvas,
                &detail_font,
                "gw: —",
                bx,
                by + 38,
                palette.text_tertiary,
            );
        }
    }

    // Mesh id (last 6 uppercased).
    if !mesh_id.is_empty() {
        let id_font = LoadedFont::new(FontFace::MonoRegular, 12);
        let id_str = format!("id: {}", last_n_upper(mesh_id, 6));
        text(
            canvas,
            &id_font,
            &id_str,
            bx,
            by + 60,
            palette.text_tertiary,
        );
    }
}

// ── Tile D — UPLINK / CLOUD ─────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn draw_uplink_tile(
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
fn uplink_status(palette: &Palette, network: &NetworkCtx) -> (Rgb888, String) {
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
fn draw_pair_drone_tile(
    canvas: &mut Canvas,
    palette: &Palette,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    ctx: &PageContext,
) {
    let code = pair_code(ctx).to_ascii_uppercase();

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

// ── early-life: HARDWARE (replaces RADIO LINK) ──────────────────────

fn draw_hardware_tile(
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

fn draw_setup_wizard_tile(
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

// ── id formatting ───────────────────────────────────────────────────

/// The last `n` characters of `s`, uppercased — the short-id form the tiles
/// use for the device-id caption and the mesh id.
fn last_n_upper(s: &str, n: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    let start = chars.len().saturating_sub(n);
    chars[start..]
        .iter()
        .collect::<String>()
        .to_ascii_uppercase()
}

/// The first `n` characters of `s`, uppercased — the truncated flight-mode form.
fn first_n_upper(s: &str, n: usize) -> String {
    s.chars().take(n).collect::<String>().to_ascii_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::palette::DARK;
    use crate::pages::PANEL_H;

    /// A fully-populated context that drives every default tile (radio link,
    /// drone, mesh, uplink).
    fn live_ctx() -> PageContext {
        let mut ctx = PageContext {
            hostname: "ados-9f2c1a".into(),
            clock: "13:47:23".into(),
            setup_finalized: true,
            completion_percent: Some(70.0),
            next_action: Some("pair with Mission Control".into()),
            ..PageContext::default()
        };
        ctx.link.rssi_dbm = Some(-67.0);
        ctx.link.bitrate_mbps = Some(20.0);
        ctx.link.fec_recovered = Some(1247);
        ctx.link.fec_lost = Some(3);
        ctx.link.channel = Some(161);
        ctx.link.tx_power_dbm = Some(5);
        ctx.link.topology = Some("host_vbus".into());
        ctx.drone.device_id = Some("drone-AABBCC42F1".into());
        ctx.drone.fc_mode = Some("STAB".into());
        ctx.drone.battery_pct = Some(87.0);
        ctx.drone.gps_sats = Some(11);
        ctx.drone.armed = Some(false);
        ctx.role.current = Some("receiver".into());
        ctx.role.configured = Some("receiver".into());
        ctx.role.mesh_capable = true;
        ctx.mesh.up = Some(true);
        ctx.mesh.peer_count = Some(3);
        ctx.mesh.selected_gateway = Some("ados-9f2c1b".into());
        ctx.mesh.partition = false;
        ctx.mesh.mesh_id = Some("12ABCD".into());
        ctx.network.uplink_type = Some("eth".into());
        ctx.network.uplink_reachable = Some(true);
        ctx.cloud.paired = false;
        ctx.cloud.pair_code = Some("7YTFC7".into());
        ctx.cloud.latency_ms = Some(12.0);
        ctx.cloud.broadcasting = true;
        ctx.pairing.code = Some("7YTFC7".into());
        ctx.system.cpu_pct = Some(22.0);
        ctx.system.ram_used_mb = Some(1234.0);
        ctx.system.ram_total_mb = Some(16384.0);
        ctx.system.temp_c = Some(47.0);
        ctx
    }

    /// Count pixels in the content region that differ from the background.
    fn inked_content_pixels(c: &Canvas, palette: &Palette) -> u32 {
        let mut n = 0;
        for y in CONTENT_Y as i32..(CONTENT_Y + CONTENT_H) as i32 {
            for x in 0..PANEL_W as i32 {
                if c.pixel(x, y) != palette.bg_primary {
                    n += 1;
                }
            }
        }
        n
    }

    /// A tile's tap opens the page behind the tile actually painted, not the
    /// grid position's default: an early-life slot drills into its own page, and
    /// the setup-wizard tile, which has nothing to drill into, takes no tap.
    #[test]
    fn tile_zones_follow_the_painted_slot() {
        let page = DashboardPage;
        let tile_actions = |ctx: &PageContext| -> Vec<HitAction> {
            let tiles = tile_rects();
            page.hit_zones(ctx)
                .into_iter()
                .filter(|z| tiles.iter().any(|t| (t.x, t.y) == (z.x, z.y)))
                .map(|z| z.action)
                .collect()
        };

        // Fresh rig: RADIO LINK, PAIR DRONE, SETUP WIZARD, UPLINK.
        let early = PageContext::default();
        assert_eq!(
            tile_actions(&early),
            vec![
                HitAction::OpenDetail("details.radio_link"),
                HitAction::OpenDetail("details.pair_drone"),
                HitAction::OpenDetail("details.uplink"),
            ]
        );
        // Five tabs follow the tiles.
        assert_eq!(page.hit_zones(&early).len(), 3 + 5);

        // Live rig: every default tile drills into its own detail.
        assert_eq!(
            tile_actions(&live_ctx()),
            vec![
                HitAction::OpenDetail("details.radio_link"),
                HitAction::OpenDetail("details.drone"),
                HitAction::OpenDetail("details.mesh"),
                HitAction::OpenDetail("details.uplink"),
            ]
        );
    }

    #[test]
    fn render_is_full_panel() {
        let page = DashboardPage;
        let ctx = PageContext::default();
        let c = page.render(&ctx, &DARK);
        assert_eq!(c.width(), PANEL_W);
        assert_eq!(c.height(), PANEL_H);
    }

    #[test]
    fn live_dashboard_renders_a_non_blank_frame() {
        let page = DashboardPage;
        let ctx = live_ctx();
        let c = page.render(&ctx, &DARK);
        // The four populated tiles ink a substantial chunk of the content
        // region: borders, titles, headline numbers, status dots, and a QR or
        // pair code. A blank frame would have zero inked content pixels.
        let inked = inked_content_pixels(&c, &DARK);
        assert!(
            inked > 2000,
            "live dashboard should ink the content region, got {inked} px"
        );
    }

    #[test]
    fn default_ctx_routes_to_early_life_slots() {
        // A fresh rig with no data routes the link slot to RADIO LINK (the
        // hardware-check has no wfb_radio row to flag), the drone slot to PAIR
        // DRONE, and the mesh slot to SETUP WIZARD (wizard not finalized).
        let slots = route_tiles(&PageContext::default());
        assert_eq!(slots[0], Slot::RadioLink);
        assert_eq!(slots[1], Slot::PairDrone);
        assert_eq!(slots[2], Slot::SetupWizard);
        assert_eq!(slots[3], Slot::Uplink);
    }

    /// The radio row is identified by the setup service's own item id. The
    /// fixture is the setup status shape that service emits, run through the
    /// state mapper, so a renamed id on either side fails here.
    #[test]
    fn missing_radio_routes_link_slot_to_hardware() {
        let setup = serde_json::json!({
            "hardware_check": {
                "profile": "ground_station",
                "items": [
                    {"id": "board", "label": "Companion compute", "required": true,
                     "state": "ok", "detail": "", "fix_hint": ""},
                    {"id": "radio_wfb", "label": "WFB radio adapter", "required": true,
                     "state": "missing", "detail": "",
                     "fix_hint": "Plug in an RTL8812EU/AU USB adapter."}
                ]
            }
        });
        let mut src = crate::state_source::StateSource::with_paths(
            "http://127.0.0.1:1",
            std::path::Path::new("/nonexistent/pairing.json"),
            "/nonexistent/hop.json".into(),
        );
        let ctx = src.compose(None, Some(&setup), None);
        let slots = route_tiles(&ctx);
        assert_eq!(slots[0], Slot::Hardware);
    }

    /// The brownout warning is a claim about the supply path, so it needs one.
    /// An unknown topology never warns, whatever the TX power.
    #[test]
    fn brownout_needs_a_declared_host_vbus_topology() {
        let mut link = LinkCtx {
            tx_power_dbm: Some(15),
            ..LinkCtx::default()
        };
        assert!(!brownout_risk(&link), "unknown topology must not warn");
        link.topology = Some("powered_hub".into());
        assert!(!brownout_risk(&link));
        link.topology = Some("host_vbus".into());
        assert!(brownout_risk(&link));
        link.tx_power_dbm = Some(BROWNOUT_TX_DBM_THRESHOLD);
        assert!(!brownout_risk(&link), "at the threshold is still safe");
    }

    /// An uplink the agent does not report is unknown, not OFFLINE; only a
    /// reported `none` is offline, and a reported Ethernet uplink shows as such.
    #[test]
    fn an_unreported_uplink_is_unknown_not_offline() {
        let mut network = NetworkCtx::default();
        let (color, label) = uplink_status(&DARK, &network);
        assert_eq!((label.as_str(), color), ("—", DARK.text_tertiary));

        network.uplink_type = Some("none".into());
        network.uplink_reachable = Some(false);
        let (color, label) = uplink_status(&DARK, &network);
        assert_eq!((label.as_str(), color), ("OFFLINE", DARK.status_error));

        network.uplink_type = Some("eth".into());
        network.uplink_reachable = Some(true);
        let (color, label) = uplink_status(&DARK, &network);
        assert_eq!((label.as_str(), color), ("eth", DARK.status_success));
    }

    #[test]
    fn live_ctx_routes_to_default_tiles() {
        let slots = route_tiles(&live_ctx());
        assert_eq!(slots[0], Slot::RadioLink);
        assert_eq!(slots[1], Slot::Drone);
        assert_eq!(slots[2], Slot::Mesh);
        assert_eq!(slots[3], Slot::Uplink);
    }

    #[test]
    fn group_thousands_inserts_separators() {
        assert_eq!(group_thousands(1247), "1,247");
        assert_eq!(group_thousands(3), "3");
        assert_eq!(group_thousands(1000000), "1,000,000");
        assert_eq!(group_thousands(0), "0");
    }

    #[test]
    fn last_and_first_n_upper() {
        // The drone tile caption is the last six characters of the device id,
        // uppercased.
        assert_eq!(last_n_upper("drone-aabbcc42f1", 6), "CC42F1");
        // A device id shorter than the window returns the whole string.
        assert_eq!(last_n_upper("ab", 6), "AB");
        // The flight-mode form is the leading six characters, uppercased.
        assert_eq!(first_n_upper("stabilize", 6), "STABIL");
        assert_eq!(first_n_upper("alt", 6), "ALT");
    }

    #[test]
    fn truncate_adds_ellipsis_when_needed() {
        // A very narrow column forces a trim; the result fits and ends with the
        // ellipsis. A wide column returns the string unchanged.
        let trimmed = truncate_to_width(FontFace::SansRegular, 11, "Open Mission Control", 30);
        assert!(trimmed.ends_with('…'));
        let whole = truncate_to_width(FontFace::SansRegular, 11, "OK", 400);
        assert_eq!(whole, "OK");
    }
}
