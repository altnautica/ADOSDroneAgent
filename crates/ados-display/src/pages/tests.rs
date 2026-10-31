use super::*;
use crate::graphics::palette::DARK;

#[test]
fn content_region_math_adds_up() {
    assert_eq!(TOP_BAR_H + CONTENT_H + BOTTOM_BAR_H, PANEL_H);
    assert_eq!(CONTENT_Y, TOP_BAR_H);
}

#[test]
fn tile_rects_form_a_2x2_grid() {
    let tiles = tile_rects();
    // Top row shares a y; bottom row shares a y.
    assert_eq!(tiles[0].y, tiles[1].y);
    assert_eq!(tiles[2].y, tiles[3].y);
    // Left column shares an x; right column shares an x.
    assert_eq!(tiles[0].x, tiles[2].x);
    assert_eq!(tiles[1].x, tiles[3].x);
    // All four tiles are the same size.
    for t in &tiles[1..] {
        assert_eq!(t.w, tiles[0].w);
        assert_eq!(t.h, tiles[0].h);
    }
    // The expected inset tile size: (480-16-8)/2 = 228, (244-16-8)/2 = 110.
    assert_eq!(tiles[0].w, 228);
    assert_eq!(tiles[0].h, 110);
}

#[test]
fn hit_zone_contains_is_half_open() {
    let z = HitZone::new(10, 10, 20, 20, HitAction::Back);
    assert!(z.contains(10, 10));
    assert!(z.contains(29, 29));
    assert!(!z.contains(30, 30));
    assert!(!z.contains(9, 9));
}

#[test]
fn blank_panel_is_full_size() {
    let c = blank_panel(&DARK);
    assert_eq!(c.width(), PANEL_W);
    assert_eq!(c.height(), PANEL_H);
}

/// The pairing QR opens Mission Control's `/pair` route with the code in
/// `code=` (the parameter that route reads) and the node's LAN name.
#[test]
fn the_pair_deep_link_targets_the_pair_route() {
    let mut ctx = PageContext::default();
    assert_eq!(
        ctx.pair_deep_link("7yt-fc7"),
        "https://command.altnautica.com/pair?code=7YTFC7"
    );
    ctx.mission_control_url = Some("https://mc.example.com/".into());
    ctx.lan_host = Some("ados-9f2c1a.local".into());
    assert_eq!(
        ctx.pair_deep_link("7YTFC7"),
        "https://mc.example.com/pair?code=7YTFC7&host=ados-9f2c1a.local"
    );
}

/// The setup URL uses a reach name the agent reported, never a literal
/// hostname; with none known there is no URL.
#[test]
fn the_setup_url_uses_only_a_reported_reach_name() {
    let mut ctx = PageContext::default();
    assert_eq!(ctx.setup_url(), None);
    ctx.network.ap_ip = Some("192.168.4.1".into());
    assert_eq!(ctx.setup_url().as_deref(), Some("http://192.168.4.1:8080"));
    ctx.lan_host = Some("ados-9f2c1a.local".into());
    assert_eq!(
        ctx.setup_url().as_deref(),
        Some("http://ados-9f2c1a.local:8080")
    );
}
