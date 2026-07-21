//! ados-oled-i2c: drive a small I2C OLED (SSD1306 / SH1106, 128x64 or 128x32)
//! with a compact ground-station status page.
//!
//! This is the wired consumer of the crate's `oled` driver. It is INDEPENDENT
//! of the framebuffer / SPI-LCD display arbitration (the legacy `ados-oled`
//! unit, which despite its name runs the framebuffer renderer): a tiny I2C
//! status OLED coexists with the HDMI cockpit, so it runs whenever an OLED
//! answers on the I2C bus, regardless of what the main display surface is.
//!
//! Auto-skip (Rule 26): if I2C is not enabled (`/dev/i2c-<bus>` absent) or no
//! device acknowledges at the configured address, the service logs the reason
//! and exits cleanly (rc 0) — a board with no OLED never churns.
//!
//! Reuse: status comes from [`ados_display::state_source::StateSource`] (the
//! same agent REST + sidecars the framebuffer UI reads); text is rendered with
//! [`ados_display::graphics::fonts::LoadedFont`] (grayscale coverage thresholded
//! to 1bpp); the frame is packed + uploaded by [`ados_display::oled`]. The frame
//! builder is pure and unit-tested; only the I2C open/write is Linux-gated.

use ados_display::graphics::fonts::LoadedFont;
use ados_display::oled::{self, Controller, OledGeometry};
use ados_display::pages::PageContext;

/// Read a `u8` env var with a fallback (for ops overrides; the defaults match a
/// standard Raspberry Pi header OLED).
fn env_u8(key: &str, default: u8) -> u8 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

/// Parse a `u16` that may be hex (`0x3c`) or decimal (`60`). Pure.
fn parse_u16(s: &str) -> Option<u16> {
    let t = s.trim();
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        u16::from_str_radix(hex, 16).ok()
    } else {
        t.parse().ok()
    }
}

/// Resolve the panel geometry from `ADOS_OLED_HEIGHT` (32 -> 128x32, else
/// 128x64, the common part).
fn geometry_from_env() -> OledGeometry {
    if env_u8("ADOS_OLED_HEIGHT", 64) == 32 {
        OledGeometry::W128_H32
    } else {
        OledGeometry::W128_H64
    }
}

/// Resolve the controller from `ADOS_OLED_CONTROLLER` (`sh1106` -> SH1106, else
/// SSD1306).
fn controller_from_env() -> Controller {
    match std::env::var("ADOS_OLED_CONTROLLER").as_deref() {
        Ok("sh1106") | Ok("SH1106") => Controller::Sh1106,
        _ => Controller::Ssd1306,
    }
}

/// The I2C addresses to probe, in order. When the operator pins `ADOS_OLED_ADDR`
/// we honour exactly that; otherwise we auto-probe the two addresses these OLED
/// modules ship at — 0x3C (default) then 0x3D (the jumpered variant) — so the
/// panel is found with zero manual config regardless of its address strap
/// (Rule 26 plug-and-play).
fn addresses_to_probe() -> Vec<u16> {
    resolve_probe_addresses(std::env::var("ADOS_OLED_ADDR").ok().as_deref())
}

/// Pure resolution of the probe address list from an optional pinned value.
fn resolve_probe_addresses(pinned: Option<&str>) -> Vec<u16> {
    match pinned {
        Some(s) => vec![parse_u16(s).unwrap_or(oled::DEFAULT_I2C_ADDR)],
        None => vec![0x3C, 0x3D],
    }
}

/// The primary LAN IPv4 the box is reachable at, or "no network". Uses the
/// connect-a-UDP-socket trick (no packet is sent) to read the source address the
/// kernel would use for an off-box route — i.e. the interface with the default
/// route.
fn primary_ip() -> String {
    use std::net::UdpSocket;
    UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("8.8.8.8:80")?;
            s.local_addr()
        })
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "no network".to_string())
}

/// Format the link line from the paired-link state + RSSI.
fn link_line(ctx: &PageContext) -> String {
    match ctx.link.state.as_deref() {
        Some(state) => match ctx.link.rssi_dbm {
            Some(rssi) => format!("link {state} {rssi:.0}dBm"),
            None => format!("link {state}"),
        },
        None => "link: idle".to_string(),
    }
}

/// Format the fourth line: pairing window > role > ready.
fn status_line(ctx: &PageContext) -> String {
    if ctx.pairing.window_active {
        let secs = ctx.pairing.window_remaining_seconds.unwrap_or(0.0) as i64;
        format!("PAIRING {secs}s")
    } else if let Some(role) = ctx.role.current.as_deref() {
        format!("role: {role}")
    } else if ctx.network.uplink_reachable {
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

/// Build the packed OLED frame for the current status. PURE (no I2C) so it is
/// unit-tested without a device. `title`/`ip`/link/status are laid out top-down
/// on `line_h`-tall rows.
fn build_frame(
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

    oled::pack_page_buffer(&canvas, geom).unwrap_or_else(|| vec![0u8; geom.frame_bytes()])
}

#[cfg(target_os = "linux")]
fn run() {
    use ados_display::graphics::fonts::FontFace;
    use ados_display::state_source::StateSource;
    use std::time::Duration;

    let bus = env_u8("ADOS_OLED_BUS", 1);
    let geom = geometry_from_env();
    let controller = controller_from_env();

    // Open the bus once and probe each candidate address. A missing
    // /dev/i2c-<bus> (I2C not enabled) OR no device acknowledging at any
    // candidate address is a clean skip (rc 0) — a board with no OLED never
    // churns. The init write is the presence check: an absent device NAKs
    // (ENXIO), so a failing init means no OLED at that address.
    let mut found: Option<(oled::I2cDev, u16)> = None;
    for addr in addresses_to_probe() {
        let mut dev = match oled::I2cDev::open(bus, addr) {
            Ok(d) => d,
            Err(e) => {
                tracing::info!(bus, addr, error = %e, "no I2C OLED bus; service exits cleanly");
                return;
            }
        };
        match oled::init(&mut dev, geom) {
            Ok(()) => {
                found = Some((dev, addr));
                break;
            }
            Err(e) => {
                tracing::debug!(bus, addr, error = %e, "no OLED acknowledged at address");
            }
        }
    }
    let (mut dev, addr) = match found {
        Some(f) => f,
        None => {
            tracing::info!(
                bus,
                "no OLED acknowledged on the I2C bus; service exits cleanly"
            );
            return;
        }
    };
    tracing::info!(
        bus,
        addr,
        height = geom.height,
        "OLED status service started"
    );

    let title_font = LoadedFont::new(FontFace::SansBold, 13);
    let body_font = LoadedFont::new(FontFace::SansRegular, 11);
    let mut src = StateSource::new();

    loop {
        let ctx = src.build_context();
        let ip = primary_ip();
        let packed = build_frame(&ctx, &ip, geom, &title_font, &body_font);
        if let Err(e) = oled::render_frame(&mut dev, &packed, geom, controller) {
            // A transient bus error should not kill the service; log and retry.
            tracing::warn!(error = %e, "OLED frame upload failed; retrying");
        }
        std::thread::sleep(Duration::from_millis(700));
    }
}

#[cfg(not(target_os = "linux"))]
fn run() {
    // This service drives a Linux I2C device. On other hosts (dev machines) do
    // a single dry render so the pure frame builder + env helpers stay compiled
    // and exercised (no dead-code warnings off-target), then exit.
    use ados_display::graphics::fonts::FontFace;
    let geom = geometry_from_env();
    let _ = controller_from_env();
    let _ = addresses_to_probe();
    let title_font = LoadedFont::new(FontFace::SansBold, 13);
    let body_font = LoadedFont::new(FontFace::SansRegular, 11);
    let frame = build_frame(
        &PageContext::default(),
        &primary_ip(),
        geom,
        &title_font,
        &body_font,
    );
    eprintln!(
        "ados-oled-i2c drives a Linux I2C device; not supported on this host ({} bytes/frame)",
        frame.len()
    );
}

/// Best-effort tracing init: journald on Linux (the unit journals to it),
/// stderr fmt otherwise, both fed to the logging daemon. Mirrors ados-display.
fn init_logging() {
    use ados_protocol::logd::layer::LogdLayer;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::EnvFilter;

    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());

    #[cfg(target_os = "linux")]
    {
        if let Ok(journald) = tracing_journald::layer() {
            let _ = tracing_subscriber::registry()
                .with(EnvFilter::new(&filter))
                .with(journald)
                .with(LogdLayer::new("ados-oled-i2c"))
                .try_init();
            return;
        }
    }

    let _ = tracing_subscriber::registry()
        .with(EnvFilter::new(&filter))
        .with(tracing_subscriber::fmt::layer())
        .with(LogdLayer::new("ados-oled-i2c"))
        .try_init();
}

fn main() {
    init_logging();
    run();
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_display::graphics::fonts::FontFace;

    fn ctx_with_host(host: &str) -> PageContext {
        PageContext {
            hostname: host.to_string(),
            ..PageContext::default()
        }
    }

    #[test]
    fn build_frame_is_the_right_size_and_paints_something() {
        let geom = OledGeometry::W128_H64;
        let title = LoadedFont::new(FontFace::SansBold, 13);
        let body = LoadedFont::new(FontFace::SansRegular, 11);
        let ctx = ctx_with_host("ados-ground");
        let frame = build_frame(&ctx, "10.0.0.5", geom, &title, &body);
        assert_eq!(frame.len(), geom.frame_bytes());
        // The hostname + IP text must set at least some pixels.
        assert!(frame.iter().any(|&b| b != 0), "frame should not be blank");
    }

    #[test]
    fn build_frame_handles_empty_hostname() {
        let geom = OledGeometry::W128_H32;
        let title = LoadedFont::new(FontFace::SansBold, 13);
        let body = LoadedFont::new(FontFace::SansRegular, 11);
        let ctx = ctx_with_host("");
        let frame = build_frame(&ctx, "no network", geom, &title, &body);
        assert_eq!(frame.len(), geom.frame_bytes());
        assert!(frame.iter().any(|&b| b != 0));
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

    #[test]
    fn parse_u16_handles_hex_and_decimal() {
        assert_eq!(parse_u16("0x3c"), Some(0x3c));
        assert_eq!(parse_u16("0X3D"), Some(0x3d));
        assert_eq!(parse_u16(" 60 "), Some(60));
        assert_eq!(parse_u16("nonsense"), None);
    }

    #[test]
    fn probe_addresses_auto_scan_both_when_unpinned() {
        // Unpinned -> auto-probe the two common OLED addresses in order.
        assert_eq!(resolve_probe_addresses(None), vec![0x3C, 0x3D]);
        // Pinned -> honour exactly that address.
        assert_eq!(resolve_probe_addresses(Some("0x3d")), vec![0x3D]);
        assert_eq!(resolve_probe_addresses(Some("60")), vec![60]);
        // A garbage pin falls back to the default single address (not a scan).
        assert_eq!(
            resolve_probe_addresses(Some("xyz")),
            vec![oled::DEFAULT_I2C_ADDR]
        );
    }
}
