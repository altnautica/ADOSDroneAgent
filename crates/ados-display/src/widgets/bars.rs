//! The top status bar and the bottom tab bar (cells, active accent, press pulse).

use embedded_graphics::pixelcolor::Rgb888;

use super::tab_icons::draw_tab_icon;
use crate::graphics::fonts::{FontFace, LoadedFont};
use crate::graphics::palette::Palette;
use crate::graphics::primitives::{fill_rect, line, text, Canvas};
use crate::graphics::status_dot::draw_dot;
use crate::graphics::thresholds;
use crate::pages::{tab_width, BOTTOM_BAR_H, PANEL_W, TOP_BAR_H};

/// Format the RAM used/total pair as `U.U/TG`, or `-` when either side is
/// absent. Gigabytes, one decimal on the used side.
pub(super) fn format_ram(used_mb: Option<f64>, total_mb: Option<f64>) -> String {
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

    // Hostname. An unresolved one is shown as unknown, never as a stand-in name.
    let name_font = LoadedFont::new(FontFace::SansBold, 14);
    let name = if hostname.is_empty() { "—" } else { hostname };
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

    let cpu_color = palette.grade(cpu_pct, thresholds::CPU_PCT);
    let ram_pct = match (ram_used_mb, ram_total_mb) {
        (Some(u), Some(t)) if t > 0.0 => Some(u / t * 100.0),
        _ => None,
    };
    let ram_color = palette.grade(ram_pct, thresholds::RAM_PCT);
    let temp_color = palette.grade(temp_c, thresholds::TEMP_C);

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
/// Paint the 44 px bottom tab bar at panel-global `(0, y)`: one cell per entry
/// of `tabs`, left to right, each [`tab_width`]`(tabs.len())` wide.
///
/// `active_tab` is the page id of the visible top-level tab; the matching cell
/// gets the primary tint plus a 2 px accent line. The navigator paints the bar
/// over every tabbed page and routes taps in it, so pages never carry tab zones.
pub fn draw_bottom_bar(canvas: &mut Canvas, palette: &Palette, tabs: &[&str], active_tab: &str) {
    let y = (canvas.height() as i32) - BOTTOM_BAR_H as i32;
    let w = PANEL_W as i32;
    let h = BOTTOM_BAR_H as i32;
    fill_rect(canvas, 0, y, w - 1, y + h - 1, palette.bg_secondary);
    line(canvas, 0, y, w - 1, y, palette.border_default);

    let tab_w = tab_width(tabs.len());
    for (i, &page_id) in tabs.iter().enumerate() {
        let tab_x0 = i as i32 * tab_w;
        let is_active = page_id == active_tab;
        let icon_color = if is_active {
            palette.text_primary
        } else {
            palette.text_tertiary
        };

        if is_active {
            // 2 px top accent line.
            fill_rect(
                canvas,
                tab_x0,
                y,
                tab_x0 + tab_w - 1,
                y + 1,
                palette.accent_primary,
            );
        }

        let cx = tab_x0 + tab_w / 2;
        let cy = y + h / 2;
        draw_tab_icon(canvas, page_id, cx, cy, icon_color);
    }
}

/// Paint the tab for `page_id` pressed: an inverse fill of its cell with the
/// icon knocked out in the background tone. Drawn over a frame that already
/// carries the bottom bar for the same `tabs`; an id not in `tabs` paints
/// nothing.
pub fn draw_tab_pulse(canvas: &mut Canvas, palette: &Palette, tabs: &[&str], page_id: &str) {
    let Some(index) = tabs.iter().position(|id| *id == page_id) else {
        return;
    };
    let tab_w = tab_width(tabs.len());
    let y = (canvas.height() as i32) - BOTTOM_BAR_H as i32;
    let x0 = index as i32 * tab_w;
    fill_rect(
        canvas,
        x0,
        y + 1,
        x0 + tab_w - 1,
        y + BOTTOM_BAR_H as i32 - 1,
        palette.accent_primary,
    );
    let (cx, cy) = (x0 + tab_w / 2, y + BOTTOM_BAR_H as i32 / 2);
    draw_tab_icon(canvas, page_id, cx, cy, palette.bg_primary);
}
