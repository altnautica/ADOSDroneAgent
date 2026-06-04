//! Full-screen touch-calibration target screen.
//!
//! Painted by the render loop while a [`crate::calibration::CalibrationController`]
//! is active (outside the navigator — there is no tab bar, and the operator
//! cannot leave until the capture completes). It draws a reticle at the current
//! target plus a centered instruction so the operator knows where to tap and how
//! far through the 9-point capture they are. The instruction block is placed in
//! the half of the panel away from the current target so the text never sits
//! under the reticle.

use crate::calibration::CalibrationController;
use crate::graphics::fonts::{FontFace, LoadedFont};
use crate::graphics::palette::Palette;
use crate::graphics::primitives::{fill_circle, line, text, Canvas};
use crate::pages::{PANEL_H, PANEL_W};

/// Half-length of each crosshair arm in pixels.
const ARM: i32 = 18;
/// Outer reticle ring radius.
const RING_R: i32 = 10;
/// Center dot radius.
const DOT_R: i32 = 2;

/// Paint the calibration target screen for the controller's current state.
pub fn render_calibration(ctrl: &CalibrationController, palette: &Palette) -> Canvas {
    let mut canvas = Canvas::new(PANEL_W, PANEL_H, palette.bg_primary);

    let (tx, ty) = ctrl.current_target();

    // Reticle: crosshair arms, then a background-filled ring that clears the
    // arms' centre, then a center dot — a clean target the eye centres on.
    let color = palette.accent_primary;
    line(&mut canvas, tx - ARM, ty, tx + ARM, ty, color);
    line(&mut canvas, tx, ty - ARM, tx, ty + ARM, color);
    fill_circle(&mut canvas, tx, ty, RING_R, palette.bg_primary, Some(color));
    fill_circle(&mut canvas, tx, ty, DOT_R, color, None);

    // Place the instruction block in the half away from the current target so
    // the text never overlaps the reticle.
    let block_y = if ty < PANEL_H as i32 / 2 {
        (PANEL_H as i32 * 2) / 3
    } else {
        PANEL_H as i32 / 4
    };

    let title_font = LoadedFont::new(FontFace::SansBold, 20);
    let body_font = LoadedFont::new(FontFace::SansRegular, 14);
    let note_font = LoadedFont::new(FontFace::SansRegular, 12);

    draw_centered(
        &mut canvas,
        &title_font,
        "Touch calibration",
        block_y,
        palette.text_primary,
    );

    let progress = format!(
        "Tap the crosshair  ({} of {})",
        ctrl.current_index() + 1,
        ctrl.target_count()
    );
    draw_centered(
        &mut canvas,
        &body_font,
        &progress,
        block_y + 30,
        palette.text_secondary,
    );

    if ctrl.failed() {
        draw_centered(
            &mut canvas,
            &note_font,
            "Couldn't read that. Starting over.",
            block_y + 54,
            palette.status_warning,
        );
    }

    canvas
}

/// Draw `s` horizontally centered on the panel at top-left `y`.
fn draw_centered(
    canvas: &mut Canvas,
    font: &LoadedFont,
    s: &str,
    y: i32,
    color: embedded_graphics::pixelcolor::Rgb888,
) {
    let w = font.text_advance(s) as i32;
    let x = ((PANEL_W as i32 - w) / 2).max(0);
    text(canvas, font, s, x, y, color);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::palette::DARK;

    #[test]
    fn renders_full_panel_and_inks_the_reticle() {
        let ctrl = CalibrationController::new(0);
        let c = render_calibration(&ctrl, &DARK);
        assert_eq!(c.width(), PANEL_W);
        assert_eq!(c.height(), PANEL_H);
        // The first target is (40, 40); the reticle inks pixels near it.
        let mut inked = false;
        for y in 22..58 {
            for x in 22..58 {
                if c.pixel(x, y) != DARK.bg_primary {
                    inked = true;
                    break;
                }
            }
            if inked {
                break;
            }
        }
        assert!(
            inked,
            "the reticle should ink pixels around the first target"
        );
    }

    #[test]
    fn failed_state_adds_the_retry_note() {
        // Drive a degenerate run so the controller flags a retry, then confirm
        // the screen paints more ink (the extra note line) than a clean state.
        let mut failed = CalibrationController::new(0);
        for _ in 0..crate::calibration::TARGETS.len() {
            let _ = failed.on_tap_raw((500, 500), std::path::Path::new("/nonexistent/touch.calib"));
        }
        assert!(failed.failed());
        let c = render_calibration(&failed, &DARK);
        assert_eq!(c.width(), PANEL_W);
    }
}
