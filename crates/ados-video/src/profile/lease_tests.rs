use super::test_support::{spawn_applier, ManualClock};
use super::*;
use crate::config::CameraConfig;
use crate::profile::base_settings;

fn control_at(base: VideoProfile, clock: &Arc<ManualClock>) -> Arc<EncoderControl> {
    let settings = base_settings(base, &CameraConfig::default());
    EncoderControl::with_clock(base, settings, clock.clone())
}

#[test]
fn a_lease_makes_a_thumbnail_drone_hero_and_is_listed() {
    let clock = ManualClock::new();
    let ctl = control_at(VideoProfile::Thumbnail, &clock);
    ctl.set_lease("gcs.op-1", Duration::from_secs(30));
    assert_eq!(ctl.desired().profile, VideoProfile::Hero);
    assert_eq!(ctl.base_profile(), VideoProfile::Thumbnail);
    assert_eq!(
        ctl.leases(),
        vec![LeaseView {
            holder: "gcs.op-1".to_string(),
            expires_in_s: Some(30.0),
        }]
    );
}

#[test]
fn a_renewal_extends_the_lease_from_the_renewal_time() {
    let clock = ManualClock::new();
    let ctl = control_at(VideoProfile::Thumbnail, &clock);
    ctl.set_lease("a", Duration::from_secs(30));
    clock.advance(Duration::from_secs(20));
    ctl.set_lease("a", Duration::from_secs(30));
    assert_eq!(ctl.leases()[0].expires_in_s, Some(30.0));
    // Past the first deadline, inside the renewed one.
    clock.advance(Duration::from_secs(20));
    assert!(ctl.sweep_expired_leases().is_none());
    assert_eq!(ctl.desired().profile, VideoProfile::Hero);
    // The renewed deadline.
    clock.advance(Duration::from_secs(10));
    assert!(ctl.sweep_expired_leases().is_some());
    assert_eq!(ctl.desired().profile, VideoProfile::Thumbnail);
}

#[test]
fn expiry_reverts_to_the_base_without_a_further_request() {
    let clock = ManualClock::new();
    let ctl = control_at(VideoProfile::Thumbnail, &clock);
    let set = ctl.set_lease("a", Duration::from_secs(10));
    clock.advance(Duration::from_secs(9));
    assert!(ctl.sweep_expired_leases().is_none());
    assert_eq!(ctl.desired().profile, VideoProfile::Hero);
    clock.advance(Duration::from_secs(1));
    let swept = ctl.sweep_expired_leases().expect("the lease expired");
    assert!(swept > set);
    assert_eq!(ctl.desired().generation, swept);
    assert_eq!(ctl.desired().profile, VideoProfile::Thumbnail);
    assert!(ctl.leases().is_empty());
}

#[test]
fn a_sweep_with_nothing_expired_publishes_no_generation() {
    let clock = ManualClock::new();
    let ctl = control_at(VideoProfile::Thumbnail, &clock);
    ctl.set_lease("a", Duration::from_secs(30));
    let before = ctl.desired().generation;
    assert!(ctl.sweep_expired_leases().is_none());
    assert_eq!(ctl.desired().generation, before);
}

#[test]
fn a_release_reverts_and_releasing_an_unknown_holder_is_harmless() {
    let clock = ManualClock::new();
    let ctl = control_at(VideoProfile::Thumbnail, &clock);
    ctl.set_lease("a", Duration::from_secs(30));
    ctl.set_lease("b", Duration::from_secs(30));
    ctl.release_lease("a");
    // Another holder still claims hero.
    assert_eq!(ctl.desired().profile, VideoProfile::Hero);
    ctl.release_lease("b");
    assert_eq!(ctl.desired().profile, VideoProfile::Thumbnail);
    ctl.release_lease("never-held");
    assert_eq!(ctl.desired().profile, VideoProfile::Thumbnail);
    assert!(ctl.leases().is_empty());
}

#[test]
fn a_plain_set_moves_the_base_while_a_live_lease_keeps_hero() {
    let clock = ManualClock::new();
    let ctl = control_at(VideoProfile::Hero, &clock);
    ctl.set_lease("viewer", Duration::from_secs(30));
    // The fleet demotes this drone while an operator is watching it.
    ctl.request_profile(VideoProfile::Thumbnail);
    assert_eq!(ctl.desired().profile, VideoProfile::Hero);
    assert_eq!(ctl.base_profile(), VideoProfile::Thumbnail);
    // The lease ends: the new base takes over.
    ctl.release_lease("viewer");
    assert_eq!(ctl.desired().profile, VideoProfile::Thumbnail);
    // With no lease, a plain set applies directly.
    ctl.request_profile(VideoProfile::Hero);
    assert_eq!(ctl.desired().profile, VideoProfile::Hero);
}

#[test]
fn a_held_lease_never_expires_and_drops_with_its_guard() {
    let clock = ManualClock::new();
    let ctl = control_at(VideoProfile::Thumbnail, &clock);
    let (guard, _) = ctl.hold_lease(VISION_LEASE_HOLDER);
    clock.advance(Duration::from_secs(3600));
    assert!(ctl.sweep_expired_leases().is_none());
    assert_eq!(ctl.desired().profile, VideoProfile::Hero);
    assert_eq!(
        ctl.leases(),
        vec![LeaseView {
            holder: VISION_LEASE_HOLDER.to_string(),
            expires_in_s: None,
        }]
    );
    drop(guard);
    assert_eq!(ctl.desired().profile, VideoProfile::Thumbnail);
    assert!(ctl.leases().is_empty());
}

#[test]
fn an_applied_state_is_stamped_with_the_base_and_the_leases() {
    let clock = ManualClock::new();
    let ctl = control_at(VideoProfile::Thumbnail, &clock);
    let generation = ctl.set_lease("a", Duration::from_secs(30));
    let hero = base_settings(VideoProfile::Hero, &CameraConfig::default());
    let stamped = ctl.note_applied(
        EncoderState::new(VideoProfile::Hero, None, hero),
        generation,
        true,
    );
    assert_eq!(stamped.profile, VideoProfile::Hero);
    assert_eq!(stamped.base_profile, Some(VideoProfile::Thumbnail));
    assert_eq!(stamped.leases.len(), 1);
    assert_eq!(ctl.applied().state, stamped);
}

#[test]
fn a_sidecar_without_the_lease_fields_still_parses() {
    let old = r#"{"profile":"hero","ceiling_kbps":null,"width":1280,"height":720,
        "fps":30,"bitrate_kbps":4000}"#;
    let st: EncoderState = serde_json::from_str(old).unwrap();
    assert_eq!(st.profile, VideoProfile::Hero);
    assert_eq!(st.base_profile, None);
    assert!(st.leases.is_empty());
}

#[test]
fn holder_ids_are_bounded_to_the_documented_charset_and_length() {
    let longest = "a".repeat(LEASE_HOLDER_MAX_LEN);
    for ok in ["a", "gcs-1", "mc:tab.7_x", "A.b_C:d-E", longest.as_str()] {
        assert_eq!(check_lease_holder(ok), Ok(()), "{ok}");
    }
    let too_long = "a".repeat(LEASE_HOLDER_MAX_LEN + 1);
    for bad in [
        "",
        "has space",
        "a/b",
        "a\"b",
        "caf\u{e9}",
        too_long.as_str(),
    ] {
        assert!(check_lease_holder(bad).is_err(), "{bad}");
    }
    // The internal vision holder cannot be claimed or released remotely.
    assert!(check_lease_holder(VISION_LEASE_HOLDER).is_err());
}

#[test]
fn lease_seconds_are_bounded_inclusively() {
    assert!(!lease_seconds_in_range(LEASE_MIN_S - 1));
    assert!(lease_seconds_in_range(LEASE_MIN_S));
    assert!(lease_seconds_in_range(LEASE_DEFAULT_S));
    assert!(lease_seconds_in_range(LEASE_MAX_S));
    assert!(!lease_seconds_in_range(LEASE_MAX_S + 1));
}

#[tokio::test]
async fn the_expiry_task_reverts_an_abandoned_lease() {
    let clock = ManualClock::new();
    let ctl = control_at(VideoProfile::Thumbnail, &clock);
    let _applier = spawn_applier(Arc::clone(&ctl));
    let generation = ctl.set_lease("gone", Duration::from_secs(5));
    let hero = ctl
        .wait_applied(generation, Duration::from_secs(2))
        .await
        .expect("the lease was applied");
    assert_eq!(hero.state.profile, VideoProfile::Hero);

    // The holder stops renewing.
    clock.advance(Duration::from_secs(6));
    let shutdown = ados_protocol::shutdown::Shutdown::new();
    let task = tokio::spawn(run_lease_expiry(Arc::clone(&ctl), shutdown.clone()));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while ctl.applied().state.profile != VideoProfile::Thumbnail
        && tokio::time::Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let reverted = ctl.applied().state;
    assert_eq!(reverted.profile, VideoProfile::Thumbnail);
    assert!(reverted.leases.is_empty());

    shutdown.trigger();
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("the expiry task stops on shutdown")
        .unwrap();
}
