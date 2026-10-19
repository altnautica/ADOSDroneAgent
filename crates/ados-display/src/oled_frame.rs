//! The I2C status OLED's frame builders: the `home` status frame (hostname,
//! LAN IP, link and status lines laid out top-down on a 1bpp canvas) and the
//! rotating ground-station service screens in [`screens`], then packed for
//! the controller.
//!
//! Home layout. A 128x64 panel paints the bold hostname then body lines for
//! the LAN IP, link and status. When a GNSS service or a radio receive plane
//! reports, a fifth service line (`gnss <fix> <sats> radios <n>`) is added and
//! the four body lines share the height under the title at an even pitch. A
//! 128x32 panel keeps the hostname and IP only.
//!
//! PURE (no I2C), so the `ados-oled-i2c` daemon, the unit tests and the
//! `render_oled` example all paint the exact same frame.

use crate::graphics::fonts::{FontFace, LoadedFont};
use crate::oled::{self, OledGeometry};
use crate::pages::gs_ctx::fix_short;
use crate::pages::PageContext;

pub mod screens;
use screens::OledScreen;

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

/// The service line: `gnss <fix> <sats>` and `radios <n>`, each present only
/// while its service reports. `None` when neither does.
pub fn service_line(ctx: &PageContext) -> Option<String> {
    let gnss = ctx.gnss.as_ref().map(|g| {
        let sats = g.sats.map_or("--".to_string(), |s| s.to_string());
        format!("gnss {} {sats}", fix_short(g.fix.as_deref()))
    });
    let radios = ctx
        .radio_adapters
        .as_ref()
        .map(|r| format!("radios {}", r.adapters.len()));
    match (gnss, radios) {
        (Some(g), Some(r)) => Some(format!("{g} {r}")),
        (g, r) => g.or(r),
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
    let mut lines = vec![ip.to_string(), link_line(ctx), status_line(ctx)];
    if let Some(service) = service_line(ctx).filter(|_| h >= 64) {
        lines.push(service);
    }
    // Body pitch: the font's line height, tightened so every line fits.
    let pitch = body_h.min((h as i32 - title_h) / lines.len() as i32);
    // Line 1: hostname (bold).
    let host = if ctx.hostname.is_empty() {
        "ADOS ground"
    } else {
        &ctx.hostname
    };
    draw_line(&mut canvas, w, h, title_font, host, 0, 0);
    // Then IP, link, status (and the service line), as far as the panel
    // has room: the IP always, the rest only while they start on-panel.
    let mut y = title_h;
    for (i, line) in lines.iter().enumerate() {
        if i > 0 && (y as usize) >= h {
            break;
        }
        draw_line(&mut canvas, w, h, body_font, line, 0, y);
        y += if h >= 64 { pitch } else { body_h };
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

/// Build the packed frame for one rotation `screen`: the status frame for
/// `home`, else the service screen's lines (falling back to the status frame
/// when the service has gone absent since the rotation was resolved).
pub fn build_screen_frame(
    screen: OledScreen,
    ctx: &PageContext,
    ip: &str,
    geom: OledGeometry,
    title_font: &LoadedFont,
    body_font: &LoadedFont,
    screen_font: &LoadedFont,
) -> Vec<u8> {
    let canvas = match screens::screen_lines(screen, ctx) {
        Some(lines) => screens::render_lines(&lines, geom, screen_font),
        None => render_canvas(ctx, ip, geom, title_font, body_font),
    };
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

    fn gs_ctx(adapters: usize) -> PageContext {
        use crate::pages::gs_ctx::{GnssCtx, RadioAdaptersCtx};
        use ados_protocol::radio_adapters::RadioAdaptersBody;
        let body: RadioAdaptersBody = serde_json::from_value(serde_json::json!({
            "present": true,
            "adapters": (0..adapters).map(|i| serde_json::json!({
                "iface": format!("wlan{i}"), "mac": null, "usb_path": null,
                "driver": "rtl88x2eu", "role": "rx", "antenna": null, "tx_active": false,
                "rssi_dbm": [-50], "packets": 100, "lost": 0, "fec_rec": null, "verified": true
            })).collect::<Vec<_>>(),
        }))
        .unwrap();
        PageContext {
            gnss: Some(GnssCtx {
                fix: Some("3d".into()),
                sats: Some(14),
                ..GnssCtx::default()
            }),
            radio_adapters: Some(RadioAdaptersCtx {
                adapters: body.adapters,
                diversity: body.diversity,
            }),
            ..ctx_with_host("gs")
        }
    }

    #[test]
    fn service_line_shows_present_services_only() {
        assert_eq!(service_line(&ctx_with_host("gs")), None);
        let mut ctx = gs_ctx(3);
        assert_eq!(service_line(&ctx).as_deref(), Some("gnss 3d 14 radios 3"));
        ctx.radio_adapters = None;
        assert_eq!(service_line(&ctx).as_deref(), Some("gnss 3d 14"));
        ctx = gs_ctx(3);
        ctx.gnss = None;
        assert_eq!(service_line(&ctx).as_deref(), Some("radios 3"));
    }

    /// The service line adds a fifth line that still lands on a 64 px panel;
    /// the bottom rows carry ink only once it is shown.
    #[test]
    fn the_service_line_fits_the_64px_panel() {
        let geom = OledGeometry::W128_H64;
        let (title, body) = status_fonts();
        let lit_rows = |ctx: &PageContext| {
            let c = render_canvas(ctx, "10.0.0.5", geom, &title, &body);
            (0..64)
                .filter(|y| c[y * 128..(y + 1) * 128].contains(&1))
                .max()
        };
        let plain = lit_rows(&ctx_with_host("gs")).unwrap();
        let with_service = lit_rows(&gs_ctx(3)).unwrap();
        assert!(with_service > plain, "{with_service} vs {plain}");
        assert!(with_service < 64);
    }
}
