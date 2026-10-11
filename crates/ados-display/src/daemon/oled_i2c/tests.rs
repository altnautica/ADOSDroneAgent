use super::*;
use crate::graphics::fonts::FontFace;

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
fn configured_brightness_reads_the_ui_oled_block() {
    let yaml = "ground_station:\n  ui:\n    oled:\n      brightness: 90\n";
    assert_eq!(configured_brightness(Some(yaml)), 90);
    assert_eq!(configured_brightness(Some("agent:\n  name: gs\n")), 204);
    assert_eq!(configured_brightness(Some(": not yaml")), 204);
    assert_eq!(configured_brightness(None), 204);
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
