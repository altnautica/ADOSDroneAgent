//! The I2C status OLED's frame builder: hostname, LAN IP, link and status lines
//! laid out top-down on a 1bpp canvas, then packed for the controller.
//!
//! PURE (no I2C), so the `ados-oled-i2c` daemon, the unit tests and the
//! `render_oled` example all paint the exact same frame.

use crate::graphics::fonts::{FontFace, LoadedFont};
use crate::oled::{self, OledGeometry};
use crate::pages::PageContext;

/// The fonts the status OLED paints with: bold 13 px title, regular 11 px body.
pub fn status_fonts() -> (LoadedFont, LoadedFont) {
    (
        LoadedFont::new(FontFace::SansBold, 13),
        LoadedFont::new(FontFace::SansRegular, 11),
    )
}

/// Format the link line from the paired-link state + RSSI.
pub fn link_line(ctx: &PageContext) -> String {
    match ctx.link.state.as_deref() {
        Some(state) => match ctx.link.rssi_dbm {
            Some(rssi) => format!("link {state} {rssi:.0}dBm"),
            None => format!("link {state}"),
        },
        None => "link: idle".to_string(),
    }
}

/// Format the fourth line: pairing window > role > ready.
pub fn status_line(ctx: &PageContext) -> String {
    if ctx.pairing.window_active {
        let secs = ctx.pairing.window_remaining_seconds.unwrap_or(0.0) as i64;
        format!("PAIRING {secs}s")
    } else if let Some(role) = ctx.role.current.as_deref() {
        format!("role: {role}")
    } else if ctx.network.uplink_reachable == Some(true) {
        "uplink up".to_string()
    } else {
        "ready".to_string()
    }
}

/// Draw one line of text onto a 1bpp canvas (grayscale coverage > 0.5 -> on),
/// clipped to the canvas bounds.
fn draw_line(canvas: &mut [u8], w: usize, h: usize, font: &LoadedFont, text: &str, x: i32, y: i32) {
    font.draw_text(text, x, y, |px, py, coverage| {
        if coverage > 0.5 && px >= 0 && py >= 0 {
            let (px, py) = (px as usize, py as usize);
            if px < w && py < h {
                canvas[py * w + px] = 1;
            }
        }
    });
}

/// Paint the status lines onto an unpacked row-major canvas, one byte per
/// pixel (1 = lit), `geom.width * geom.height` long.
pub fn render_canvas(
    ctx: &PageContext,
    ip: &str,
    geom: OledGeometry,
    title_font: &LoadedFont,
    body_font: &LoadedFont,
) -> Vec<u8> {
    let w = geom.width as usize;
    let h = geom.height as usize;
    let mut canvas = vec![0u8; w * h];

    let title_h = title_font.line_height() as i32;
    let body_h = body_font.line_height() as i32;
    let mut y = 0;
    // Line 1: hostname (bold).
    let host = if ctx.hostname.is_empty() {
        "ADOS ground"
    } else {
        &ctx.hostname
    };
    draw_line(&mut canvas, w, h, title_font, host, 0, y);
    y += title_h;
    // Line 2: LAN IP.
    draw_line(&mut canvas, w, h, body_font, ip, 0, y);
    y += body_h;
    // Line 3: link (only room for it / line 4 on a 64px panel).
    if (y as usize) < h {
        draw_line(&mut canvas, w, h, body_font, &link_line(ctx), 0, y);
        y += body_h;
    }
    // Line 4: pairing / role / ready.
    if (y as usize) < h {
        draw_line(&mut canvas, w, h, body_font, &status_line(ctx), 0, y);
    }
    canvas
}

/// Build the packed OLED frame for the current status, ready for upload.
pub fn build_frame(
    ctx: &PageContext,
    ip: &str,
    geom: OledGeometry,
    title_font: &LoadedFont,
    body_font: &LoadedFont,
) -> Vec<u8> {
    let canvas = render_canvas(ctx, ip, geom, title_font, body_font);
    oled::pack_page_buffer(&canvas, geom).unwrap_or_else(|| vec![0u8; geom.frame_bytes()])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_with_host(host: &str) -> PageContext {
        PageContext {
            hostname: host.to_string(),
            ..PageContext::default()
        }
    }

    #[test]
    fn build_frame_is_the_right_size_and_paints_something() {
        let geom = OledGeometry::W128_H64;
        let (title, body) = status_fonts();
        let ctx = ctx_with_host("ados-ground");
        let frame = build_frame(&ctx, "10.0.0.5", geom, &title, &body);
        assert_eq!(frame.len(), geom.frame_bytes());
        // The hostname + IP text must set at least some pixels.
        assert!(frame.iter().any(|&b| b != 0), "frame should not be blank");
    }

    #[test]
    fn build_frame_handles_empty_hostname() {
        let geom = OledGeometry::W128_H32;
        let (title, body) = status_fonts();
        let ctx = ctx_with_host("");
        let frame = build_frame(&ctx, "no network", geom, &title, &body);
        assert_eq!(frame.len(), geom.frame_bytes());
        assert!(frame.iter().any(|&b| b != 0));
    }

    #[test]
    fn render_canvas_matches_the_panel_geometry() {
        let geom = OledGeometry::W128_H64;
        let (title, body) = status_fonts();
        let canvas = render_canvas(&ctx_with_host("gs"), "no network", geom, &title, &body);
        assert_eq!(canvas.len(), 128 * 64);
        assert!(canvas.iter().all(|&p| p <= 1));
    }

    #[test]
    fn link_line_reflects_state() {
        let mut ctx = ctx_with_host("gs");
        assert_eq!(link_line(&ctx), "link: idle");
        ctx.link.state = Some("connected".to_string());
        ctx.link.rssi_dbm = Some(-58.0);
        assert_eq!(link_line(&ctx), "link connected -58dBm");
    }

    #[test]
    fn status_line_prioritises_pairing() {
        let mut ctx = ctx_with_host("gs");
        assert_eq!(status_line(&ctx), "ready");
        ctx.role.current = Some("direct".to_string());
        assert_eq!(status_line(&ctx), "role: direct");
        ctx.pairing.window_active = true;
        ctx.pairing.window_remaining_seconds = Some(42.0);
        assert_eq!(status_line(&ctx), "PAIRING 42s");
    }
}
