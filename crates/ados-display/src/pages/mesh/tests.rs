use super::*;
use crate::graphics::palette::DARK;
use crate::pages::{MeshPeer, PANEL_W};

#[test]
fn mesh_renders_with_peer_zones_when_closed() {
    let page = MeshDetailPage::new();
    let mut ctx = PageContext::default();
    ctx.role.current = Some("receiver".to_string());
    ctx.mesh.up = Some(true);
    ctx.mesh.peers = Some(
        (0..3)
            .map(|i| MeshPeer {
                device_id: Some(format!("ados-peer-{i:08x}")),
                role: Some("relay".to_string()),
                last_seen_seconds_ago: Some(i as f64 * 2.0),
            })
            .collect(),
    );
    let c = page.render(&ctx, &DARK);
    assert_eq!(c.width(), PANEL_W);
    let zones = page.hit_zones(&ctx);
    // Back + switch; the peer rows are a read-out.
    assert_eq!(zones.len(), 2);
    assert_eq!(zones[0].action, HitAction::Back);
    assert_eq!(
        zones[1].action,
        HitAction::Custom("mesh.switch_role".to_string())
    );
}

#[test]
fn picker_swaps_the_zone_set() {
    let page = MeshDetailPage::new();
    let ctx = PageContext::default();
    page.toggle_picker();
    assert!(page.picker_open());
    let c = page.render(&ctx, &DARK);
    assert_eq!(c.width(), PANEL_W);
    let zones = page.hit_zones(&ctx);
    // Back + switch + three role choices.
    assert_eq!(zones.len(), 5);
    assert_eq!(
        zones[2].action,
        HitAction::Custom("mesh.role.direct".to_string())
    );
    assert_eq!(
        zones[4].action,
        HitAction::Custom("mesh.role.receiver".to_string())
    );
    page.close_picker();
    assert!(!page.picker_open());
}

/// Switch opens the picker; a first choice tap only arms it, the
/// confirming tap closes the picker and writes that role.
#[test]
fn a_role_choice_takes_two_taps() {
    let page = MeshDetailPage::new();
    let ctx = PageContext::default();
    assert_eq!(
        page.on_custom("mesh.switch_role", &ctx),
        Some(PanelAction::Repaint)
    );
    assert!(page.picker_open());
    assert_eq!(
        page.on_custom("mesh.role.relay", &ctx),
        Some(PanelAction::Repaint)
    );
    assert!(page.picker_open(), "an armed choice keeps the picker open");
    let Some(PanelAction::Agent(req)) = page.on_custom("mesh.role.relay", &ctx) else {
        panic!("the confirming tap must reach the agent");
    };
    assert_eq!(
        (req.method, req.path),
        ("PUT", "/api/v1/ground-station/role")
    );
    assert_eq!(req.body, Some(serde_json::json!({"role": "relay"})));
    assert!(!page.picker_open());
    assert!(page.on_custom("mesh.role.bogus", &ctx).is_none());
}

/// Arming one role and then tapping another re-arms the second rather
/// than writing either; closing the picker drops the arm.
#[test]
fn a_different_choice_or_reopen_rearms() {
    let page = MeshDetailPage::new();
    let ctx = PageContext::default();
    page.toggle_picker();
    assert_eq!(
        page.on_custom("mesh.role.relay", &ctx),
        Some(PanelAction::Repaint)
    );
    assert_eq!(
        page.on_custom("mesh.role.receiver", &ctx),
        Some(PanelAction::Repaint)
    );
    // Close and reopen: the receiver arm is gone.
    page.toggle_picker();
    page.toggle_picker();
    assert_eq!(
        page.on_custom("mesh.role.receiver", &ctx),
        Some(PanelAction::Repaint)
    );
    assert!(matches!(
        page.on_custom("mesh.role.receiver", &ctx),
        Some(PanelAction::Agent(_))
    ));
}

#[test]
fn direct_role_renders_notice() {
    let page = MeshDetailPage::new();
    let mut ctx = PageContext::default();
    ctx.role.current = Some("direct".to_string());
    let c = page.render(&ctx, &DARK);
    assert_eq!(c.width(), PANEL_W);
}

#[test]
fn scroll_never_goes_negative() {
    let page = MeshDetailPage::new();
    page.scroll_by(-5);
    assert_eq!(page.scroll_offset.get(), 0);
    page.scroll_by(3);
    assert_eq!(page.scroll_offset.get(), 3);
}
