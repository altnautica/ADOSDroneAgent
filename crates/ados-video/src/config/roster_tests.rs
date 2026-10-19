use super::*;

#[test]
fn the_primary_camera_resolves_to_main_and_the_others_to_their_own_ids() {
    let cfg = RosterVideoConfig::from_text(
        "\
agent:
  profile: drone
cameras:
  - { id: ir, source: rtsp://pod/ir, role: ir }
  - { id: eo, source: /dev/video2, role: primary }
  - { id: nav, source: /dev/video0, capture: true }
",
    );
    assert_eq!(cfg.published_leg("eo"), Some(PRIMARY_LEG));
    assert_eq!(cfg.published_leg("ir"), Some("ir"));
    assert_eq!(cfg.published_leg("nav"), Some("nav"));
}

#[test]
fn anything_but_a_configured_enabled_camera_resolves_to_nothing() {
    let cfg = RosterVideoConfig::from_text(
        "\
cameras:
  - { id: eo, source: /dev/video2 }
  - { id: belly, source: /dev/video4, enabled: false }
",
    );
    assert_eq!(cfg.published_leg("eo"), Some(PRIMARY_LEG));
    // `main` is a media path, not a camera of this roster.
    assert_eq!(cfg.published_leg("main"), None);
    assert_eq!(cfg.published_leg("belly"), None);
    for other in ["", "missing", "../eo", "eo/whep"] {
        assert_eq!(cfg.published_leg(other), None, "{other}");
    }
}

#[test]
fn a_node_without_a_cameras_list_serves_its_single_camera_as_main() {
    let cfg = RosterVideoConfig::from_text("agent:\n  profile: drone\nvideo:\n  mode: wfb\n");
    assert_eq!(cfg.published_leg("main"), Some(PRIMARY_LEG));
    assert_eq!(cfg.published_leg("eo"), None);
}

#[test]
fn a_ground_station_publishes_no_camera() {
    let cfg = RosterVideoConfig::from_text(
        "\
agent:
  profile: ground_station
cameras:
  - { id: eo, source: /dev/video2 }
",
    );
    assert_eq!(cfg.published_leg("eo"), None);
    assert_eq!(cfg.published_leg("main"), None);
}
