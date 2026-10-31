//! The 24 px tab-bar icon glyphs.

use embedded_graphics::pixelcolor::Rgb888;

use crate::graphics::primitives::{fill_rect, line, Canvas};

/// Paint a 24 px icon for `icon` centered in a tab cell at `(cx, cy)`.
///
/// The icons are line-drawn to match the tab semantics the bitmap set carried:
/// a 2x2 grid for the dashboard, a film frame for video, a gear ring for
/// settings, a bar trio for link stats, and a step line for channel hops; the
/// visor tabs get a plug for the plugin page, a camera body for cameras, a
/// pulse trace for sensors, a disk stack for storage, and an "i" for about.
pub(super) fn draw_tab_icon(canvas: &mut Canvas, icon: &str, cx: i32, cy: i32, color: Rgb888) {
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
        "plugin" => {
            // A plug: two prongs over a body with a cord below.
            fill_rect(canvas, cx - 5, cy - r, cx - 3, cy - 4, color);
            fill_rect(canvas, cx + 3, cy - r, cx + 5, cy - 4, color);
            fill_rect(canvas, cx - 7, cy - 3, cx + 7, cy + 4, color);
            fill_rect(canvas, cx - 1, cy + 5, cx + 1, cy + r, color);
        }
        "cameras" => {
            // A camera body: outline box, a viewfinder bump, a solid lens.
            line(canvas, cx - r, cy - 5, cx + r, cy - 5, color);
            line(canvas, cx - r, cy + 8, cx + r, cy + 8, color);
            line(canvas, cx - r, cy - 5, cx - r, cy + 8, color);
            line(canvas, cx + r, cy - 5, cx + r, cy + 8, color);
            fill_rect(canvas, cx - 4, cy - 8, cx + 2, cy - 6, color);
            crate::graphics::primitives::fill_circle(canvas, cx, cy + 1, 4, color, None);
        }
        "sensors" => {
            // A pulse trace: flat, a sharp spike, flat.
            line(canvas, cx - r, cy + 2, cx - 4, cy + 2, color);
            line(canvas, cx - 4, cy + 2, cx - 1, cy - 8, color);
            line(canvas, cx - 1, cy - 8, cx + 2, cy + 8, color);
            line(canvas, cx + 2, cy + 8, cx + 5, cy + 2, color);
            line(canvas, cx + 5, cy + 2, cx + r, cy + 2, color);
        }
        "storage" => {
            // A disk stack: three slabs with gaps between them.
            for k in 0..3 {
                let y0 = cy - r + k * 7;
                fill_rect(canvas, cx - 8, y0, cx + 8, y0 + 4, color);
            }
        }
        "details.about" => {
            // An information mark: a dot over a stem.
            fill_rect(canvas, cx - 1, cy - 9, cx + 1, cy - 7, color);
            fill_rect(canvas, cx - 1, cy - 4, cx + 1, cy + 8, color);
            line(canvas, cx - 4, cy - 4, cx - 1, cy - 4, color);
            line(canvas, cx - 4, cy + 8, cx + 4, cy + 8, color);
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
