//! The DRONE and MESH tiles.

use super::text::{first_n_upper, last_n_upper};
use crate::graphics::fonts::{FontFace, LoadedFont};
use crate::graphics::palette::Palette;
use crate::graphics::primitives::{text, Canvas};
use crate::graphics::status_dot::draw_dot;
use crate::graphics::thresholds;
use crate::pages::{ArmState, DroneCtx, MeshCtx, MeshState, RoleCtx};
use crate::widgets::{draw_big_number, draw_tile};

#[allow(clippy::too_many_arguments)]
pub(super) fn draw_drone_tile(
    canvas: &mut Canvas,
    palette: &Palette,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    drone: &DroneCtx,
    code: &str,
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
pub(super) fn draw_mesh_tile(
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
