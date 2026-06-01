//! Composite chrome widgets shared across pages.
//!
//! Where [`crate::graphics`] holds the narrow draw primitives (rect, line,
//! text, sparkline, bar, dot, QR), this module holds the larger reusable chrome
//! the pages compose from: the top status bar (hostname, role badge, system
//! metrics, clock), the bottom tab bar (five tabs with icons + active accent),
//! the bordered content tile, and the headline big number with a unit suffix.
//!
//! The geometry, colors, fonts, and text match the on-device layout one for one
//! so a render is pixel-faithful regardless of which side produces it.

use embedded_graphics::pixelcolor::Rgb888;

use crate::graphics::fonts::{FontFace, LoadedFont};
use crate::graphics::palette::{Palette, ThresholdDirection};
use crate::graphics::primitives::{fill_rect, fill_rect_outline, line, text, Canvas};
use crate::graphics::status_dot::draw_dot;
use crate::pages::{HitAction, HitZone, BOTTOM_BAR_H, PANEL_W, TOP_BAR_H};

/// Height reserved for a tile's caps title row.
pub const TILE_TITLE_BAR_H: i32 = 18;
/// Horizontal padding inside a tile's title row.
pub const TILE_TITLE_PAD_X: i32 = 8;

/// The inner body box a tile reserves below its title bar:
/// `(body_x, body_y, body_w, body_h)`.
pub type TileBody = (i32, i32, i32, i32);

// The chrome helpers carry the panel layout's full parameter sets (a tile's
// rect plus its two captions, a big number's value plus color/size/unit). The
// extra arguments are the layout contract, not accidental sprawl, so the
// argument-count lint is opted out per helper at its definition.

/// Format the RAM used/total pair as `U.U/TG`, or `-` when either side is
/// absent. Gigabytes, one decimal on the used side.
fn format_ram(used_mb: Option<f64>, total_mb: Option<f64>) -> String {
    match (used_mb, total_mb) {
        (Some(used), Some(total)) if total > 0.0 => {
            format!("{:.1}/{:.0}G", used / 1024.0, total / 1024.0)
        }
        _ => "-".to_string(),
    }
}

/// Resolve the role badge color from the role string.
fn role_color(palette: &Palette, role: &str) -> Rgb888 {
    match role.to_ascii_lowercase().as_str() {
        "receiver" => palette.status_success,
        "relay" => palette.accent_primary,
        "direct" => palette.text_secondary,
        "unset" => palette.status_warning,
        _ => palette.text_secondary,
    }
}

/// Paint the 32 px top status bar onto `canvas` at panel-global `(0, 0)`.
///
/// Left to right: hostname (Sans Bold 14), role badge (dot + uppercase label),
/// system metrics (CPU + RAM + temperature, threshold-colored), and the wall
/// clock right-anchored. Closes with a 1 px divider on the bottom edge.
#[allow(clippy::too_many_arguments)]
pub fn draw_top_bar(
    canvas: &mut Canvas,
    palette: &Palette,
    hostname: &str,
    role: &str,
    cpu_pct: Option<f64>,
    ram_used_mb: Option<f64>,
    ram_total_mb: Option<f64>,
    temp_c: Option<f64>,
    clock: &str,
) {
    let w = PANEL_W as i32;
    let h = TOP_BAR_H as i32;
    fill_rect(canvas, 0, 0, w - 1, h - 1, palette.bg_primary);

    // Hostname.
    let name_font = LoadedFont::new(FontFace::SansBold, 14);
    let name = if hostname.is_empty() {
        "groundnode"
    } else {
        hostname
    };
    text(canvas, &name_font, name, 8, 8, palette.text_primary);
    let name_w = name_font.text_size(name).0 as i32;

    // Role badge: dot + uppercase label.
    let role_current = if role.is_empty() { "unset" } else { role };
    let role_lower = role_current.to_ascii_lowercase();
    let badge_color = role_color(palette, &role_lower);
    let role_upper = role_lower.to_ascii_uppercase();
    let role_x = 8 + name_w + 14;
    let dot_cx = role_x + 6;
    let dot_cy = h / 2;
    draw_dot(canvas, dot_cx, dot_cy, badge_color, 5, palette.bg_primary);
    let role_font = LoadedFont::new(FontFace::SansBold, 12);
    let label_x = role_x + 16;
    text(
        canvas,
        &role_font,
        &role_upper,
        label_x,
        9,
        palette.text_primary,
    );
    let role_w = role_font.text_size(&role_upper).0 as i32;

    // System metrics: CPU + RAM + temperature.
    let label_font = LoadedFont::new(FontFace::MonoRegular, 11);
    let value_font = LoadedFont::new(FontFace::MonoRegular, 11);
    let text_y = 10;
    let mut cursor_x = label_x + role_w + 18;

    let cpu_color = palette.threshold_color(cpu_pct, 70.0, 85.0, ThresholdDirection::LowerIsBetter);
    let ram_pct = match (ram_used_mb, ram_total_mb) {
        (Some(u), Some(t)) if t > 0.0 => Some(u / t * 100.0),
        _ => None,
    };
    let ram_color = palette.threshold_color(ram_pct, 70.0, 85.0, ThresholdDirection::LowerIsBetter);
    let temp_color = palette.threshold_color(temp_c, 65.0, 75.0, ThresholdDirection::LowerIsBetter);

    let mut emit = |cursor: &mut i32, label: &str, value: &str, color: Rgb888| {
        text(
            canvas,
            &label_font,
            label,
            *cursor,
            text_y,
            palette.text_tertiary,
        );
        *cursor += label_font.text_size(label).0 as i32 + 4;
        text(canvas, &value_font, value, *cursor, text_y, color);
        *cursor += value_font.text_size(value).0 as i32 + 12;
    };

    let cpu_str = match cpu_pct {
        Some(v) => format!("{}%", v as i64),
        None => "-".to_string(),
    };
    emit(&mut cursor_x, "CPU", &cpu_str, cpu_color);
    emit(
        &mut cursor_x,
        "RAM",
        &format_ram(ram_used_mb, ram_total_mb),
        ram_color,
    );
    let temp_str = match temp_c {
        Some(v) => format!("{}°C", v as i64),
        None => "-".to_string(),
    };
    emit(&mut cursor_x, "T", &temp_str, temp_color);

    // Wall clock right-anchored.
    let clock_font = LoadedFont::new(FontFace::MonoRegular, 13);
    let clock_w = clock_font.text_size(clock).0 as i32;
    text(
        canvas,
        &clock_font,
        clock,
        w - 8 - clock_w,
        9,
        palette.text_secondary,
    );

    // 1 px divider on the bottom edge.
    line(canvas, 0, h - 1, w - 1, h - 1, palette.border_default);
}

/// One bottom-bar tab: stable zone id, the page id it routes to, and the icon to
/// paint. Five tabs share the 480 px width at 96 px each.
const TABS: [(&str, &str); 5] = [
    ("dashboard", "dashboard"),
    ("video", "video"),
    ("settings", "settings"),
    ("link_stats", "link_stats"),
    ("channel_hops", "channel_hops"),
];

/// Number of bottom-bar tabs.
pub const TAB_COUNT: i32 = 5;
/// Width of one bottom-bar tab.
pub const TAB_WIDTH: i32 = PANEL_W as i32 / TAB_COUNT;

/// Paint a 24 px icon for `icon` centered in a tab cell at `(cx, cy)`.
///
/// The icons are line-drawn to match the tab semantics the bitmap set carried:
/// a 2x2 grid for the dashboard, a film frame for video, a gear ring for
/// settings, a bar trio for link stats, and a step line for channel hops.
fn draw_tab_icon(canvas: &mut Canvas, icon: &str, cx: i32, cy: i32, color: Rgb888) {
    let r = 10; // half the 20 px glyph box inside the 24 px icon
    match icon {
        "dashboard" => {
            // Four rounded cells (a 2x2 grid).
            fill_rect(canvas, cx - r, cy - r, cx - 2, cy - 2, color);
            fill_rect(canvas, cx + 2, cy - r, cx + r, cy - 2, color);
            fill_rect(canvas, cx - r, cy + 2, cx - 2, cy + r, color);
            fill_rect(canvas, cx + 2, cy + 2, cx + r, cy + r, color);
        }
        "video" => {
            // A film frame: outline box + a centered triangle play glyph.
            line(canvas, cx - r, cy - 7, cx + r, cy - 7, color);
            line(canvas, cx - r, cy + 7, cx + r, cy + 7, color);
            line(canvas, cx - r, cy - 7, cx - r, cy + 7, color);
            line(canvas, cx + r, cy - 7, cx + r, cy + 7, color);
            for dy in -4i32..=4 {
                let half = 4 - dy.abs();
                line(canvas, cx - 2, cy + dy, cx - 2 + half, cy + dy, color);
            }
        }
        "settings" => {
            // A gear: a ring plus eight radial teeth.
            crate::graphics::primitives::fill_circle(canvas, cx, cy, 5, color, None);
            fill_rect(
                canvas,
                cx - 1,
                cy - 1,
                cx + 1,
                cy + 1,
                palette_hole(canvas, cx, cy),
            );
            for k in 0..8 {
                let ang = (k as f64) * std::f64::consts::PI / 4.0;
                let x0 = cx + (ang.cos() * 6.0).round() as i32;
                let y0 = cy + (ang.sin() * 6.0).round() as i32;
                let x1 = cx + (ang.cos() * 9.0).round() as i32;
                let y1 = cy + (ang.sin() * 9.0).round() as i32;
                line(canvas, x0, y0, x1, y1, color);
            }
        }
        "link_stats" => {
            // Three ascending bars.
            fill_rect(canvas, cx - 8, cy + 2, cx - 4, cy + r, color);
            fill_rect(canvas, cx - 2, cy - 2, cx + 2, cy + r, color);
            fill_rect(canvas, cx + 4, cy - r, cx + 8, cy + r, color);
        }
        "channel_hops" => {
            // A step-after line.
            line(canvas, cx - 9, cy + 5, cx - 3, cy + 5, color);
            line(canvas, cx - 3, cy + 5, cx - 3, cy - 2, color);
            line(canvas, cx - 3, cy - 2, cx + 3, cy - 2, color);
            line(canvas, cx + 3, cy - 2, cx + 3, cy + 8, color);
            line(canvas, cx + 3, cy + 8, cx + 9, cy + 8, color);
        }
        _ => {
            crate::graphics::primitives::fill_circle(canvas, cx, cy, 4, color, None);
        }
    }
}

/// Read the current pixel under a gear's hub so the hub punch-out keeps the tab
/// background rather than guessing a color.
fn palette_hole(canvas: &Canvas, cx: i32, cy: i32) -> Rgb888 {
    // Sample just outside the gear ring where the tab background still shows.
    canvas.pixel(cx + 12, cy)
}

/// Paint the 44 px bottom tab bar at panel-global `(0, y)` and return the five
/// tab hit zones (in page-local content coordinates: the bar sits below the
/// content region, so the zones carry their panel-global y mapped to the
/// content-local frame by the navigator).
///
/// `active_tab` is the page id of the visible top-level tab; the matching tab
/// gets the primary tint plus a 2 px accent line. The returned zones use a
/// [`HitAction::GoTab`] keyed to each tab's page id.
pub fn draw_bottom_bar(canvas: &mut Canvas, palette: &Palette, active_tab: &str) -> Vec<HitZone> {
    let y = (canvas.height() as i32) - BOTTOM_BAR_H as i32;
    let w = PANEL_W as i32;
    let h = BOTTOM_BAR_H as i32;
    fill_rect(canvas, 0, y, w - 1, y + h - 1, palette.bg_secondary);
    line(canvas, 0, y, w - 1, y, palette.border_default);

    let mut zones = Vec::with_capacity(TABS.len());
    for (i, (page_id, icon)) in TABS.iter().enumerate() {
        let tab_x0 = i as i32 * TAB_WIDTH;
        let is_active = *page_id == active_tab;
        let icon_color = if is_active {
            palette.text_primary
        } else {
            palette.text_tertiary
        };

        if is_active {
            // 2 px top accent line.
            line(
                canvas,
                tab_x0,
                y,
                tab_x0 + TAB_WIDTH - 1,
                y,
                palette.accent_primary,
            );
            line(
                canvas,
                tab_x0,
                y + 1,
                tab_x0 + TAB_WIDTH - 1,
                y + 1,
                palette.accent_primary,
            );
        }

        let cx = tab_x0 + TAB_WIDTH / 2;
        let cy = y + h / 2;
        draw_tab_icon(canvas, icon, cx, cy, icon_color);

        // Zones live in the page-local content frame; the bar is below the
        // content region so its y is mapped into the same coordinate space the
        // navigator dispatches against.
        zones.push(HitZone::new(
            tab_x0,
            y - TOP_BAR_H as i32,
            TAB_WIDTH,
            h,
            HitAction::GoTab(page_id_for(page_id)),
        ));
    }
    zones
}

/// Return the bottom tab bar's five hit zones without painting.
///
/// `hit_zones` implementations call this so the tab targets stay in sync with
/// [`draw_bottom_bar`] without re-running the paint. Zones are in the page-local
/// content frame: the bar sits directly below the 480x244 content region.
pub fn bottom_bar_zones() -> Vec<HitZone> {
    let bar_top = (PANEL_H_CONTENT) as i32; // page-local y just below the content region
    TABS.iter()
        .enumerate()
        .map(|(i, (page_id, _))| {
            HitZone::new(
                i as i32 * TAB_WIDTH,
                bar_top,
                TAB_WIDTH,
                BOTTOM_BAR_H as i32,
                HitAction::GoTab(page_id_for(page_id)),
            )
        })
        .collect()
}

/// Page-local height of the content region (panel minus both chrome bars).
const PANEL_H_CONTENT: u32 = crate::pages::CONTENT_H;

/// Return the static page id for a tab name (lets the zone carry a `'static`
/// route id without leaking allocations).
fn page_id_for(page_id: &str) -> &'static str {
    match page_id {
        "dashboard" => "dashboard",
        "video" => "video",
        "settings" => "settings",
        "link_stats" => "link_stats",
        "channel_hops" => "channel_hops",
        _ => "dashboard",
    }
}

/// Paint a bordered content tile at `(x, y, w, h)` and return its inner body box.
///
/// The tile is a 1 px-bordered box filled with `bg_secondary`, an uppercase
/// muted title in the top `TILE_TITLE_BAR_H` px, an optional right-aligned mono
/// caption, and a 1 px separator under the title. The returned body box is the
/// region below the separator with 8 px side padding.
#[allow(clippy::too_many_arguments)]
pub fn draw_tile(
    canvas: &mut Canvas,
    palette: &Palette,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    title: &str,
    title_right: &str,
) -> TileBody {
    fill_rect_outline(
        canvas,
        x,
        y,
        x + w - 1,
        y + h - 1,
        palette.bg_secondary,
        palette.border_default,
    );
    let title_font = LoadedFont::new(FontFace::SansBold, 11);
    let upper = title.to_ascii_uppercase();
    text(
        canvas,
        &title_font,
        &upper,
        x + TILE_TITLE_PAD_X,
        y + 4,
        palette.text_secondary,
    );
    if !title_right.is_empty() {
        let right_font = LoadedFont::new(FontFace::MonoRegular, 11);
        let rw = right_font.text_size(title_right).0 as i32;
        text(
            canvas,
            &right_font,
            title_right,
            x + w - TILE_TITLE_PAD_X - rw,
            y + 4,
            palette.text_tertiary,
        );
    }
    let sep_y = y + TILE_TITLE_BAR_H;
    line(
        canvas,
        x + 1,
        sep_y,
        x + w - 2,
        sep_y,
        palette.border_default,
    );

    let body_x = x + 8;
    let body_y = sep_y + 2;
    let body_w = w - 16;
    let body_h = h - TILE_TITLE_BAR_H - 10;
    (body_x, body_y, body_w, body_h)
}

/// Paint a headline number with an optional unit suffix and return the total
/// width painted.
///
/// The value renders in Mono Bold at `size`; the unit renders smaller (one
/// third the size, floored at 10 px) baseline-aligned just to the right. An
/// empty unit string skips the suffix.
#[allow(clippy::too_many_arguments)]
pub fn draw_big_number(
    canvas: &mut Canvas,
    x: i32,
    y: i32,
    value_text: &str,
    color: Rgb888,
    size: u32,
    unit: &str,
    unit_color: Rgb888,
) -> i32 {
    let value_font = LoadedFont::new(FontFace::MonoBold, size);
    text(canvas, &value_font, value_text, x, y, color);
    let (value_w, value_h) = value_font.text_size(value_text);
    let mut total_w = value_w as i32;

    if !unit.is_empty() {
        let unit_px = (size / 3).max(10);
        let unit_font = LoadedFont::new(FontFace::SansBold, unit_px);
        let (unit_w, unit_h) = unit_font.text_size(unit);
        let unit_x = x + value_w as i32 + 4;
        let unit_y = y + value_h as i32 - unit_h as i32 - 2;
        text(canvas, &unit_font, unit, unit_x, unit_y, unit_color);
        total_w += 4 + unit_w as i32;
    }
    total_w
}

/// Paint a small caps label in muted tertiary text — the column / field label
/// the detail grids share.
pub fn draw_label(canvas: &mut Canvas, palette: &Palette, x: i32, y: i32, label: &str, size: u32) {
    let font = LoadedFont::new(FontFace::SansBold, size);
    text(
        canvas,
        &font,
        &label.to_ascii_uppercase(),
        x,
        y,
        palette.text_tertiary,
    );
}

/// Height of a detail page's header band.
pub const DETAIL_HEADER_H: i32 = 40;

/// Paint a detail page's header band: a left-pointing back chevron, a centered
/// uppercase title (Sans Bold 14), and a 1 px divider under the band. Returns
/// the back-chevron hit zone (page-local coordinates).
///
/// The chevron is two 2 px diagonal strokes meeting at a left-pointing apex
/// inside a 40x32 box at `(8, 8)` — the same geometry every detail page shares.
pub fn draw_detail_header(canvas: &mut Canvas, palette: &Palette, title: &str) -> HitZone {
    let (bx, by, bw, bh) = (8, 8, 40, 32);
    let cx = bx + bw / 2;
    let cy = by + bh / 2;
    let arm = 8;
    let color = palette.text_primary;
    // Two-pixel chevron: stroke the line twice, one row apart.
    for off in 0..2 {
        line(canvas, cx - arm, cy + off, cx + arm, cy - arm + off, color);
        line(canvas, cx - arm, cy + off, cx + arm, cy + arm + off, color);
    }

    let font = LoadedFont::new(FontFace::SansBold, 14);
    let upper = title.to_ascii_uppercase();
    let tw = font.text_size(&upper).0 as i32;
    text(
        canvas,
        &font,
        &upper,
        (PANEL_W as i32 - tw) / 2,
        12,
        palette.text_primary,
    );

    line(
        canvas,
        0,
        DETAIL_HEADER_H,
        PANEL_W as i32 - 1,
        DETAIL_HEADER_H,
        palette.border_default,
    );

    HitZone::new(bx, by, bw, bh, HitAction::Back)
}

/// The detail-page back-chevron hit zone, without painting. Detail page
/// `hit_zones` implementations call this so the zone stays in sync with the one
/// [`draw_detail_header`] paints.
pub fn detail_back_zone() -> HitZone {
    HitZone::new(8, 8, 40, 32, HitAction::Back)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::palette::DARK;
    use crate::pages::blank_panel;

    #[test]
    fn format_ram_renders_gigabytes() {
        assert_eq!(format_ram(Some(1234.0), Some(16384.0)), "1.2/16G");
        assert_eq!(format_ram(None, Some(16384.0)), "-");
        assert_eq!(format_ram(Some(1234.0), Some(0.0)), "-");
    }

    #[test]
    fn top_bar_inks_the_band() {
        let mut c = blank_panel(&DARK);
        draw_top_bar(
            &mut c,
            &DARK,
            "groundnode",
            "receiver",
            Some(22.0),
            Some(1234.0),
            Some(16384.0),
            Some(47.0),
            "13:47:23",
        );
        // The divider row is the border color at the bottom edge of the bar.
        assert_eq!(c.pixel(0, TOP_BAR_H as i32 - 1), DARK.border_default);
    }

    #[test]
    fn bottom_bar_returns_five_zones() {
        let mut c = blank_panel(&DARK);
        let zones = draw_bottom_bar(&mut c, &DARK, "dashboard");
        assert_eq!(zones.len(), 5);
        assert_eq!(zones[0].action, HitAction::GoTab("dashboard"));
        assert_eq!(zones[4].action, HitAction::GoTab("channel_hops"));
        // The tabs tile the full panel width.
        assert_eq!(zones[0].w * TAB_COUNT, PANEL_W as i32);
    }

    #[test]
    fn tile_body_sits_below_the_title_bar() {
        let mut c = blank_panel(&DARK);
        let (bx, by, bw, bh) = draw_tile(&mut c, &DARK, 8, 8, 228, 110, "Radio link", "ch 161");
        assert_eq!(bx, 16);
        assert_eq!(by, 8 + TILE_TITLE_BAR_H + 2);
        assert_eq!(bw, 228 - 16);
        assert_eq!(bh, 110 - TILE_TITLE_BAR_H - 10);
    }

    #[test]
    fn big_number_reports_a_positive_width() {
        let mut c = blank_panel(&DARK);
        let w = draw_big_number(
            &mut c,
            16,
            40,
            "87",
            DARK.status_success,
            28,
            "%",
            DARK.text_secondary,
        );
        assert!(w > 0);
    }
}
