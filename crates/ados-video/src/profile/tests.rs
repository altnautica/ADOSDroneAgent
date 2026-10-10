use super::*;

fn cfg() -> CameraConfig {
    CameraConfig::default()
}

#[test]
fn hero_is_the_existing_camera_defaults_and_thumbnail_is_the_small_profile() {
    let c = cfg();
    assert_eq!(
        base_settings(VideoProfile::Hero, &c),
        EncoderSettings {
            width: 1280,
            height: 720,
            fps: 30,
            bitrate_kbps: 4000
        }
    );
    assert_eq!(
        base_settings(VideoProfile::Thumbnail, &c),
        EncoderSettings {
            width: 320,
            height: 180,
            fps: 1,
            bitrate_kbps: 50
        }
    );
}

#[test]
fn a_wfb_node_boots_thumbnail_and_an_off_channel_node_boots_hero() {
    // The fleet case: 24 aircraft powering up together must not each grab
    // 48% airtime.
    assert_eq!(boot_profile("wfb"), VideoProfile::Thumbnail);
    // A rig that shares no channel keeps today's behaviour exactly.
    assert_eq!(boot_profile("cloud"), VideoProfile::Hero);
    assert_eq!(boot_profile("disabled"), VideoProfile::Hero);
}

#[test]
fn the_ceiling_only_ever_reduces_the_bitrate() {
    let c = cfg();
    // A hero on a bad link is clamped down to the rescue tier.
    assert_eq!(
        resolve(VideoProfile::Hero, Some(1200), &c).bitrate_kbps,
        1200
    );
    // A thumbnail is NOT raised to the rescue tier.
    assert_eq!(
        resolve(VideoProfile::Thumbnail, Some(1200), &c).bitrate_kbps,
        50
    );
    // A ceiling above the profile's nominal value is a no-op.
    assert_eq!(
        resolve(VideoProfile::Hero, Some(9000), &c).bitrate_kbps,
        4000
    );
    // No ceiling is the profile's nominal value.
    assert_eq!(resolve(VideoProfile::Hero, None, &c).bitrate_kbps, 4000);
}

#[test]
fn the_ceiling_never_touches_geometry_or_frame_rate() {
    let c = cfg();
    let clamped = resolve(VideoProfile::Hero, Some(200), &c);
    assert_eq!(
        (clamped.width, clamped.height, clamped.fps),
        (1280, 720, 30)
    );
}

#[test]
fn profile_wire_strings_round_trip_and_reject_typos() {
    for p in [VideoProfile::Hero, VideoProfile::Thumbnail] {
        assert_eq!(VideoProfile::parse(p.as_str()), Some(p));
    }
    assert_eq!(VideoProfile::parse("Hero"), None);
    assert_eq!(VideoProfile::parse("thumb"), None);
    assert_eq!(VideoProfile::parse(""), None);
}

#[test]
fn the_two_setters_compose_without_clobbering_each_other() {
    let ctl = EncoderControl::new(VideoProfile::Thumbnail, cfg().thumbnail);
    let g1 = ctl.request_ceiling(Some(1200));
    let g2 = ctl.request_profile(VideoProfile::Hero);
    assert!(g2 > g1);
    let d = ctl.desired();
    // The profile request did not wipe the ceiling, nor vice versa.
    assert_eq!(d.profile, VideoProfile::Hero);
    assert_eq!(d.ceiling_kbps, Some(1200));
    assert_eq!(
        resolve(d.profile, d.ceiling_kbps, &cfg()).bitrate_kbps,
        1200
    );
}

#[tokio::test]
async fn a_waiter_unblocks_only_once_its_own_generation_is_applied() {
    let ctl = EncoderControl::new(VideoProfile::Thumbnail, cfg().thumbnail);
    let g = ctl.request_profile(VideoProfile::Hero);
    // Nothing applied yet: the wait must not resolve.
    assert!(ctl
        .wait_applied(g, Duration::from_millis(20))
        .await
        .is_none());
    let hero = base_settings(VideoProfile::Hero, &cfg());
    ctl.note_applied(EncoderState::new(VideoProfile::Hero, None, hero), g, true);
    let applied = ctl
        .wait_applied(g, Duration::from_millis(20))
        .await
        .unwrap();
    assert_eq!(applied.state.profile, VideoProfile::Hero);
    assert_eq!(applied.state.bitrate_kbps, 4000);
    assert!(applied.restarted);
}

#[test]
fn the_sidecar_round_trips_and_always_carries_the_ceiling_field() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("video-profile.json");
    let st = EncoderState::new(
        VideoProfile::Hero,
        None,
        base_settings(VideoProfile::Hero, &cfg()),
    );
    write_sidecar(&path, &st).unwrap();
    let raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(raw["profile"], "hero");
    // `ados-radio` self-heals off this key; it must exist even when clear.
    assert!(raw
        .get("ceiling_kbps")
        .is_some_and(serde_json::Value::is_null));
    assert_eq!(read_state_from(&path).unwrap(), st);
    // No temp file left behind.
    assert!(!path.with_extension("json.tmp").exists());
}
