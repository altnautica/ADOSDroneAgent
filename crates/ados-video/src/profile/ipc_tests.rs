use super::*;
use crate::config::CameraConfig;
use crate::profile::base_settings;
use crate::profile::lease::test_support::spawn_applier;

fn control() -> Arc<EncoderControl> {
    let cfg = CameraConfig::default();
    EncoderControl::new(VideoProfile::Thumbnail, cfg.thumbnail)
}

#[tokio::test]
async fn a_profile_set_answers_with_the_settings_the_applier_confirmed() {
    let ctl = control();
    let _applier = spawn_applier(Arc::clone(&ctl));
    let reply = dispatch(
        br#"{"op":"video.encoder.profile.set","profile":"hero"}"#,
        &ctl,
    )
    .await;
    assert_eq!(reply["ok"], true);
    assert_eq!(reply["applied"], true);
    assert_eq!(reply["profile"], "hero");
    assert_eq!(reply["width"], 1280);
    assert_eq!(reply["height"], 720);
    assert_eq!(reply["fps"], 30);
    assert_eq!(reply["bitrate_kbps"], 4000);
}

#[tokio::test]
async fn a_ceiling_set_clamps_the_active_profile_and_null_clears_it() {
    let ctl = control();
    let _applier = spawn_applier(Arc::clone(&ctl));
    dispatch(
        br#"{"op":"video.encoder.profile.set","profile":"hero"}"#,
        &ctl,
    )
    .await;
    let clamped = dispatch(
        br#"{"op":"video.encoder.ceiling.set","bitrate_kbps":1200}"#,
        &ctl,
    )
    .await;
    assert_eq!(clamped["bitrate_kbps"], 1200);
    assert_eq!(clamped["ceiling_kbps"], 1200);
    // Geometry is the profile's, untouched by the clamp.
    assert_eq!(clamped["width"], 1280);
    let cleared = dispatch(br#"{"op":"video.encoder.ceiling.set"}"#, &ctl).await;
    assert_eq!(cleared["bitrate_kbps"], 4000);
    assert!(cleared["ceiling_kbps"].is_null());
}

#[tokio::test]
async fn an_unconfirmed_set_reports_applied_false_instead_of_claiming_success() {
    // No applier running: the orchestrator never acknowledges.
    let ctl = control();
    let generation = ctl.request_profile(VideoProfile::Hero);
    // Drive `settle` directly with a short wait so the test stays fast; the
    // production bound is APPLY_TIMEOUT.
    assert!(ctl
        .wait_applied(generation, Duration::from_millis(20))
        .await
        .is_none());
    let a = ctl.applied();
    let reply = ok_reply(&a.state, &ctl, false, false);
    assert_eq!(reply["ok"], true);
    assert_eq!(reply["applied"], false);
    // It reports what is actually live (still the boot thumbnail), not the
    // hero settings it was asked for.
    assert_eq!(reply["profile"], "thumbnail");
}

#[tokio::test]
async fn a_lease_makes_a_thumbnail_drone_hero_and_a_release_reverts_it() {
    let ctl = control();
    let _applier = spawn_applier(Arc::clone(&ctl));
    let leased = dispatch(
        br#"{"op":"video.encoder.lease.set","holder":"gcs-1","lease_s":30}"#,
        &ctl,
    )
    .await;
    assert_eq!(leased["ok"], true);
    assert_eq!(leased["applied"], true);
    assert_eq!(leased["profile"], "hero");
    assert_eq!(leased["base_profile"], "thumbnail");
    assert_eq!(leased["width"], 1280);
    assert_eq!(leased["leases"][0]["holder"], "gcs-1");
    let left = leased["leases"][0]["expires_in_s"].as_f64().unwrap();
    assert!(left > 0.0 && left <= 30.0, "{left}");

    let got = dispatch(br#"{"op":"video.encoder.get"}"#, &ctl).await;
    assert_eq!(got["profile"], "hero");
    assert_eq!(got["base_profile"], "thumbnail");
    assert_eq!(got["leases"].as_array().unwrap().len(), 1);

    let released = dispatch(
        br#"{"op":"video.encoder.lease.release","holder":"gcs-1"}"#,
        &ctl,
    )
    .await;
    assert_eq!(released["ok"], true);
    assert_eq!(released["profile"], "thumbnail");
    assert_eq!(released["width"], 320);
    assert_eq!(released["leases"], json!([]));

    // Releasing a holder that holds nothing is not an error.
    let again = dispatch(
        br#"{"op":"video.encoder.lease.release","holder":"gcs-1"}"#,
        &ctl,
    )
    .await;
    assert_eq!(again["ok"], true);
    assert_eq!(again["profile"], "thumbnail");
}

#[tokio::test]
async fn a_plain_set_under_a_live_lease_moves_only_the_base() {
    let ctl = control();
    let _applier = spawn_applier(Arc::clone(&ctl));
    dispatch(
        br#"{"op":"video.encoder.lease.set","holder":"gcs-1","lease_s":30}"#,
        &ctl,
    )
    .await;
    let promoted = dispatch(
        br#"{"op":"video.encoder.profile.set","profile":"hero"}"#,
        &ctl,
    )
    .await;
    assert_eq!(promoted["base_profile"], "hero");
    let demoted = dispatch(
        br#"{"op":"video.encoder.profile.set","profile":"thumbnail"}"#,
        &ctl,
    )
    .await;
    assert_eq!(demoted["profile"], "hero");
    assert_eq!(demoted["base_profile"], "thumbnail");
}

#[tokio::test]
async fn bad_requests_are_rejected_without_touching_the_encoder() {
    let ctl = control();
    let before = ctl.desired();
    for body in [
        br#"{"op":"video.encoder.profile.set","profile":"Hero"}"#.as_slice(),
        br#"{"op":"video.encoder.lease.set","lease_s":30}"#.as_slice(),
        br#"{"op":"video.encoder.lease.set","holder":"","lease_s":30}"#.as_slice(),
        br#"{"op":"video.encoder.lease.set","holder":"a b","lease_s":30}"#.as_slice(),
        br#"{"op":"video.encoder.lease.set","holder":"vision","lease_s":30}"#.as_slice(),
        br#"{"op":"video.encoder.lease.set","holder":"gcs-1"}"#.as_slice(),
        br#"{"op":"video.encoder.lease.set","holder":"gcs-1","lease_s":4}"#.as_slice(),
        br#"{"op":"video.encoder.lease.set","holder":"gcs-1","lease_s":121}"#.as_slice(),
        br#"{"op":"video.encoder.lease.set","holder":"gcs-1","lease_s":30.5}"#.as_slice(),
        br#"{"op":"video.encoder.lease.set","holder":"gcs-1","lease_s":"30"}"#.as_slice(),
        br#"{"op":"video.encoder.lease.release"}"#.as_slice(),
        br#"{"op":"video.encoder.lease.release","holder":"vision"}"#.as_slice(),
        br#"{"op":"video.encoder.profile.set"}"#.as_slice(),
        br#"{"op":"video.encoder.ceiling.set","bitrate_kbps":-5}"#.as_slice(),
        br#"{"op":"video.encoder.ceiling.set","bitrate_kbps":0}"#.as_slice(),
        br#"not json"#.as_slice(),
    ] {
        let reply = dispatch(body, &ctl).await;
        assert_eq!(reply["ok"], false, "body {}", String::from_utf8_lossy(body));
        assert_eq!(reply["error"], "E_ARGS");
    }
    let unknown = dispatch(br#"{"op":"video.encoder.nope"}"#, &ctl).await;
    assert_eq!(unknown["error"], "E_UNKNOWN_OP");
    assert_eq!(ctl.desired(), before);
    assert!(ctl.leases().is_empty());
}

#[tokio::test]
async fn the_socket_round_trip_reaches_the_real_server() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("video-encoder.sock");
    let ctl = control();
    let _applier = spawn_applier(Arc::clone(&ctl));
    let shutdown = ados_protocol::shutdown::Shutdown::new();
    let server = {
        let ctl = Arc::clone(&ctl);
        let sock = sock.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move { serve(ctl, &sock, shutdown).await })
    };
    // Wait for the bind.
    for _ in 0..100 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let st = set_profile_at(&sock, VideoProfile::Hero).await.unwrap();
    assert_eq!(st.profile, VideoProfile::Hero);
    assert_eq!(
        st.settings(),
        base_settings(VideoProfile::Hero, &CameraConfig::default())
    );

    let clamped = set_bitrate_ceiling_at(&sock, Some(900)).await.unwrap();
    assert_eq!(clamped.bitrate_kbps, 900);
    assert_eq!(clamped.ceiling_kbps, Some(900));

    let read_back = state_at(&sock).await.unwrap();
    assert_eq!(read_back, clamped);

    let leased = set_lease_at(&sock, "gcs-1", 30).await.unwrap();
    assert_eq!(leased.profile, VideoProfile::Hero);
    assert_eq!(leased.base_profile, Some(VideoProfile::Hero));
    assert_eq!(leased.leases.len(), 1);
    assert_eq!(leased.leases[0].holder, "gcs-1");
    let released = release_lease_at(&sock, "gcs-1").await.unwrap();
    assert!(released.leases.is_empty());
    assert!(set_lease_at(&sock, "gcs-1", 500).await.is_err());

    shutdown.trigger();
    let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
}

#[test]
fn an_absent_or_corrupt_sidecar_reads_as_none_not_a_panic() {
    let dir = tempfile::tempdir().unwrap();
    assert!(read_state_from(&dir.path().join("missing.json")).is_none());
    let bad = dir.path().join("bad.json");
    std::fs::write(&bad, b"{not json").unwrap();
    assert!(read_state_from(&bad).is_none());
}
