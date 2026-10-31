use super::cloud_tiles::uplink_status;
use super::link_tile::brownout_risk;
use super::text::{first_n_upper, group_thousands, last_n_upper, truncate_to_width};
use super::*;
use crate::graphics::fonts::FontFace;
use crate::graphics::palette::DARK;
use crate::pages::{LinkCtx, NetworkCtx, PANEL_H};

/// A fully-populated context that drives every default tile (radio link,
/// drone, mesh, uplink).
fn live_ctx() -> PageContext {
    let mut ctx = PageContext {
        hostname: "ados-9f2c1a".into(),
        clock: "13:47:23".into(),
        setup_finalized: true,
        completion_percent: Some(70.0),
        next_action: Some("pair with Mission Control".into()),
        ..PageContext::default()
    };
    ctx.link.rssi_dbm = Some(-67.0);
    ctx.link.bitrate_kbps = Some(20000.0);
    ctx.link.fec_recovered = Some(1247);
    ctx.link.fec_lost = Some(3);
    ctx.link.channel = Some(161);
    ctx.link.tx_power_dbm = Some(5);
    ctx.link.topology = Some("host_vbus".into());
    ctx.drone.device_id = Some("drone-AABBCC42F1".into());
    ctx.drone.fc_mode = Some("STAB".into());
    ctx.drone.battery_pct = Some(87.0);
    ctx.drone.gps_sats = Some(11);
    ctx.drone.armed = Some(false);
    ctx.role.current = Some("receiver".into());
    ctx.role.configured = Some("receiver".into());
    ctx.role.mesh_capable = true;
    ctx.mesh.up = Some(true);
    ctx.mesh.peer_count = Some(3);
    ctx.mesh.selected_gateway = Some("ados-9f2c1b".into());
    ctx.mesh.partition = false;
    ctx.mesh.mesh_id = Some("12ABCD".into());
    ctx.network.uplink_type = Some("eth".into());
    ctx.network.uplink_reachable = Some(true);
    ctx.cloud.paired = false;
    ctx.cloud.pair_code = Some("7YTFC7".into());
    ctx.cloud.latency_ms = Some(12.0);
    ctx.cloud.broadcasting = true;
    ctx.pairing.code = Some("7YTFC7".into());
    ctx.system.cpu_pct = Some(22.0);
    ctx.system.ram_used_mb = Some(1234.0);
    ctx.system.ram_total_mb = Some(16384.0);
    ctx.system.temp_c = Some(47.0);
    ctx
}

/// Count pixels in the content region that differ from the background.
fn inked_content_pixels(c: &Canvas, palette: &Palette) -> u32 {
    let mut n = 0;
    for y in CONTENT_Y as i32..(CONTENT_Y + CONTENT_H) as i32 {
        for x in 0..PANEL_W as i32 {
            if c.pixel(x, y) != palette.bg_primary {
                n += 1;
            }
        }
    }
    n
}

/// A tile's tap opens the page behind the tile actually painted, not the
/// grid position's default: an early-life slot drills into its own page, and
/// the setup-wizard tile, which has nothing to drill into, takes no tap.
#[test]
fn tile_zones_follow_the_painted_slot() {
    let page = DashboardPage;
    let tile_actions = |ctx: &PageContext| -> Vec<HitAction> {
        let tiles = tile_rects();
        page.hit_zones(ctx)
            .into_iter()
            .filter(|z| tiles.iter().any(|t| (t.x, t.y) == (z.x, z.y)))
            .map(|z| z.action)
            .collect()
    };

    // Fresh rig: RADIO LINK, PAIR DRONE, SETUP WIZARD, UPLINK.
    let early = PageContext::default();
    assert_eq!(
        tile_actions(&early),
        vec![
            HitAction::OpenDetail("details.radio_link"),
            HitAction::OpenDetail("details.pair_drone"),
            HitAction::OpenDetail("details.uplink"),
        ]
    );
    // Only the tiles: the navigator owns the tab bar.
    assert_eq!(page.hit_zones(&early).len(), 3);

    // Live rig: every default tile drills into its own detail.
    assert_eq!(
        tile_actions(&live_ctx()),
        vec![
            HitAction::OpenDetail("details.radio_link"),
            HitAction::OpenDetail("details.drone"),
            HitAction::OpenDetail("details.mesh"),
            HitAction::OpenDetail("details.uplink"),
        ]
    );
}

#[test]
fn render_is_full_panel() {
    let page = DashboardPage;
    let ctx = PageContext::default();
    let c = page.render(&ctx, &DARK);
    assert_eq!(c.width(), PANEL_W);
    assert_eq!(c.height(), PANEL_H);
}

#[test]
fn live_dashboard_renders_a_non_blank_frame() {
    let page = DashboardPage;
    let ctx = live_ctx();
    let c = page.render(&ctx, &DARK);
    // The four populated tiles ink a substantial chunk of the content
    // region: borders, titles, headline numbers, status dots, and a QR or
    // pair code. A blank frame would have zero inked content pixels.
    let inked = inked_content_pixels(&c, &DARK);
    assert!(
        inked > 2000,
        "live dashboard should ink the content region, got {inked} px"
    );
}

#[test]
fn default_ctx_routes_to_early_life_slots() {
    // A fresh rig with no data routes the link slot to RADIO LINK (the
    // hardware-check has no wfb_radio row to flag), the drone slot to PAIR
    // DRONE, and the mesh slot to SETUP WIZARD (wizard not finalized).
    let slots = route_tiles(&PageContext::default());
    assert_eq!(slots[0], Slot::RadioLink);
    assert_eq!(slots[1], Slot::PairDrone);
    assert_eq!(slots[2], Slot::SetupWizard);
    assert_eq!(slots[3], Slot::Uplink);
}

/// The radio row is identified by the setup service's own item id. The
/// fixture is the setup status shape that service emits, run through the
/// state mapper, so a renamed id on either side fails here.
#[test]
fn missing_radio_routes_link_slot_to_hardware() {
    let setup = serde_json::json!({
        "hardware_check": {
            "profile": "ground_station",
            "items": [
                {"id": "board", "label": "Companion compute", "required": true,
                 "state": "ok", "detail": "", "fix_hint": ""},
                {"id": "radio_wfb", "label": "WFB radio adapter", "required": true,
                 "state": "missing", "detail": "",
                 "fix_hint": "Plug in an RTL8812EU/AU USB adapter."}
            ]
        }
    });
    let mut src = crate::state_source::StateSource::with_paths(
        "http://127.0.0.1:1",
        std::path::Path::new("/nonexistent/pairing.json"),
        "/nonexistent/hop.json".into(),
    );
    let ctx = src.compose(None, Some(&setup), None);
    let slots = route_tiles(&ctx);
    assert_eq!(slots[0], Slot::Hardware);
}

/// The brownout warning is a claim about the supply path, so it needs one.
/// An unknown topology never warns, whatever the TX power.
#[test]
fn brownout_needs_a_declared_host_vbus_topology() {
    let mut link = LinkCtx {
        tx_power_dbm: Some(15),
        ..LinkCtx::default()
    };
    assert!(!brownout_risk(&link), "unknown topology must not warn");
    link.topology = Some("powered_hub".into());
    assert!(!brownout_risk(&link));
    link.topology = Some("host_vbus".into());
    assert!(brownout_risk(&link));
    link.tx_power_dbm = Some(BROWNOUT_TX_DBM_THRESHOLD);
    assert!(!brownout_risk(&link), "at the threshold is still safe");
}

/// An uplink the agent does not report is unknown, not OFFLINE; only a
/// reported `none` is offline, and a reported Ethernet uplink shows as such.
#[test]
fn an_unreported_uplink_is_unknown_not_offline() {
    let mut network = NetworkCtx::default();
    let (color, label) = uplink_status(&DARK, &network);
    assert_eq!((label.as_str(), color), ("—", DARK.text_tertiary));

    network.uplink_type = Some("none".into());
    network.uplink_reachable = Some(false);
    let (color, label) = uplink_status(&DARK, &network);
    assert_eq!((label.as_str(), color), ("OFFLINE", DARK.status_error));

    network.uplink_type = Some("eth".into());
    network.uplink_reachable = Some(true);
    let (color, label) = uplink_status(&DARK, &network);
    assert_eq!((label.as_str(), color), ("eth", DARK.status_success));
}

#[test]
fn live_ctx_routes_to_default_tiles() {
    let slots = route_tiles(&live_ctx());
    assert_eq!(slots[0], Slot::RadioLink);
    assert_eq!(slots[1], Slot::Drone);
    assert_eq!(slots[2], Slot::Mesh);
    assert_eq!(slots[3], Slot::Uplink);
}

#[test]
fn group_thousands_inserts_separators() {
    assert_eq!(group_thousands(1247), "1,247");
    assert_eq!(group_thousands(3), "3");
    assert_eq!(group_thousands(1000000), "1,000,000");
    assert_eq!(group_thousands(0), "0");
}

#[test]
fn last_and_first_n_upper() {
    // The drone tile caption is the last six characters of the device id,
    // uppercased.
    assert_eq!(last_n_upper("drone-aabbcc42f1", 6), "CC42F1");
    // A device id shorter than the window returns the whole string.
    assert_eq!(last_n_upper("ab", 6), "AB");
    // The flight-mode form is the leading six characters, uppercased.
    assert_eq!(first_n_upper("stabilize", 6), "STABIL");
    assert_eq!(first_n_upper("alt", 6), "ALT");
}

#[test]
fn truncate_adds_ellipsis_when_needed() {
    // A very narrow column forces a trim; the result fits and ends with the
    // ellipsis. A wide column returns the string unchanged.
    let trimmed = truncate_to_width(FontFace::SansRegular, 11, "Open Mission Control", 30);
    assert!(trimmed.ends_with('…'));
    let whole = truncate_to_width(FontFace::SansRegular, 11, "OK", 400);
    assert_eq!(whole, "OK");
}
