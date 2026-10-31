//! The RADIO LINK tile and its brownout and supply-topology badges.

use embedded_graphics::pixelcolor::Rgb888;

use super::text::{group_thousands, measure};
use super::{BITRATE_CAP_MBPS, BROWNOUT_TX_DBM_THRESHOLD};
use crate::graphics::bar_meter::draw_bar;
use crate::graphics::fonts::{FontFace, LoadedFont};
use crate::graphics::palette::Palette;
use crate::graphics::primitives::{fill_rect, text, Canvas};
use crate::graphics::thresholds;
use crate::pages::{LinkCtx, PageContext};
use crate::widgets::{draw_big_number, draw_tile};

/// Whether the radio tile warns of a brownout: TX past the safe envelope on a
/// radio whose declared supply path is host VBUS. An unknown supply path never
/// warns; the warning is a statement about the topology, so it needs one.
pub(super) fn brownout_risk(link: &LinkCtx) -> bool {
    link.topology.as_deref() == Some("host_vbus")
        && link
            .tx_power_dbm
            .is_some_and(|p| p > BROWNOUT_TX_DBM_THRESHOLD)
}

// ── Tile A — RADIO LINK ─────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
pub(super) fn draw_radio_link_tile(
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
    let bitrate = link.bitrate_mbps();
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
