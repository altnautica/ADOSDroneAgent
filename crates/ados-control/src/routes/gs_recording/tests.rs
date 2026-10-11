use super::*;
use std::path::Path;

/// Read a response body as JSON.
async fn body_json(resp: Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[test]
fn error_status_maps_every_recorder_code() {
    // 409: the already/not-active codes.
    assert_eq!(error_status("E_RECORDING_ACTIVE"), StatusCode::CONFLICT);
    assert_eq!(error_status("E_RECORDING_NOT_ACTIVE"), StatusCode::CONFLICT);
    // 503: ffmpeg-missing / spawn-failed / unwritable-dir.
    assert_eq!(
        error_status("E_FFMPEG_NOT_FOUND"),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        error_status("E_RECORDER_SPAWN_FAILED"),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        error_status("E_RECORDING_DIR_UNWRITABLE"),
        StatusCode::SERVICE_UNAVAILABLE
    );
    // 507: disk full.
    assert_eq!(error_status("E_DISK_FULL").as_u16(), 507);
    // Anything else degrades to 500.
    assert_eq!(
        error_status("E_SOMETHING_ELSE"),
        StatusCode::INTERNAL_SERVER_ERROR
    );
}

#[tokio::test]
async fn recorder_error_body_is_the_error_object_detail_shape() {
    // The 409 already-recording body the start route returns.
    let resp = recorder_error(&RecorderError {
        code: "E_RECORDING_ACTIVE".to_string(),
        message: "a recording is already in progress".to_string(),
    });
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = body_json(resp).await;
    assert_eq!(
        body,
        json!({"detail": {"error": {
            "code": "E_RECORDING_ACTIVE",
            "message": "a recording is already in progress",
        }}})
    );
}

#[tokio::test]
async fn the_node_local_stop_answers_without_a_profile_gate() {
    // Nothing is recording, so the honest answer is the 409 not-active body,
    // never the ground-station profile 404 or a fabricated success.
    let resp = post_video_record_stop().await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(
        body_json(resp).await["detail"]["error"]["code"],
        json!("E_RECORDING_NOT_ACTIVE")
    );
}

#[tokio::test]
async fn disk_full_error_body_is_a_507() {
    let resp = recorder_error(&RecorderError {
        code: "E_DISK_FULL".to_string(),
        message: "less than 64 MiB free on the recordings volume".to_string(),
    });
    assert_eq!(resp.status().as_u16(), 507);
    let body = body_json(resp).await;
    assert_eq!(body["detail"]["error"]["code"], json!("E_DISK_FULL"));
}

#[tokio::test]
async fn ffmpeg_missing_error_body_is_a_503() {
    let resp = recorder_error(&RecorderError {
        code: "E_FFMPEG_NOT_FOUND".to_string(),
        message: "ffmpeg binary not on PATH".to_string(),
    });
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = body_json(resp).await;
    assert_eq!(
        body["detail"]["error"]["message"],
        json!("ffmpeg binary not on PATH")
    );
}

#[tokio::test]
async fn profile_mismatch_body_is_the_fastapi_404_shape() {
    let resp = profile_mismatch();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = body_json(resp).await;
    assert_eq!(
        body,
        json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})
    );
}

#[test]
fn start_request_filename_hint_defaults_to_none() {
    // An empty body parses with no hint (the recorder then uses a bare
    // timestamp filename), matching the FastAPI optional field.
    let req: RecordingStartRequest = serde_json::from_str("{}").unwrap();
    assert_eq!(req.filename_hint, None);
    let req2: RecordingStartRequest =
        serde_json::from_str(r#"{"filename_hint":"pipe-run"}"#).unwrap();
    assert_eq!(req2.filename_hint.as_deref(), Some("pipe-run"));
}

#[test]
fn the_singleton_returns_the_same_instance() {
    // Two calls hand back the same recorder (the Arc is cloned, not rebuilt),
    // so a start on one request and a stop on another share the same child.
    let a = recorder();
    let b = recorder();
    assert!(
        Arc::ptr_eq(&a, &b),
        "the recorder singleton must be one instance across calls"
    );
}

/// Serialises the tests that put a fake `ffmpeg` on `PATH`.
///
/// `PATH` is process-global and the test harness runs these on separate
/// threads, so two of them mutating it concurrently would make each other
/// flaky. A `tokio::sync::Mutex` (not a `std` one) because the guard is held
/// across the recorder's awaits.
static PATH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A directory holding an `ffmpeg` that just sleeps, so a real capture can be
/// driven into the running state without an encoder or a rig — the same
/// technique `ados-video`'s own recorder cycle test uses.
fn fake_ffmpeg_dir() -> tempfile::TempDir {
    let bindir = tempfile::tempdir().unwrap();
    let fake = bindir.path().join("ffmpeg");
    std::fs::write(&fake, "#!/bin/sh\nsleep 30\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bindir
}

/// Prepend `dir` to `PATH`, returning the previous value to restore.
fn path_prepend(dir: &Path) -> Option<String> {
    let saved = std::env::var("PATH").ok();
    let combined = match &saved {
        Some(orig) => format!("{}:{}", dir.display(), orig),
        None => dir.display().to_string(),
    };
    std::env::set_var("PATH", &combined);
    saved
}

fn path_restore(saved: Option<String>) {
    match saved {
        Some(p) => std::env::set_var("PATH", p),
        None => std::env::remove_var("PATH"),
    }
}

/// The recording legs must follow the recorder, both ways.
///
/// These two fields were hardcoded `false`/`null`, so the ground-station
/// status surface denied a capture that was in flight. The active leg is what
/// regressed, so it is what this drives.
#[tokio::test]
async fn recording_view_follows_the_recorder_in_both_states() {
    let _guard = PATH_LOCK.lock().await;
    let bindir = fake_ffmpeg_dir();
    let recdir = tempfile::tempdir().unwrap();
    let rec = GroundStationRecorder::new(recdir.path(), "rtsp://127.0.0.1:8554/main");

    // Idle: no capture, so no filename either.
    assert_eq!(recording_view(&rec).await, (false, Value::Null));

    let saved = path_prepend(bindir.path());
    let started = rec.start(Some("statusleg")).await.expect("start succeeds");

    let (active, filename) = recording_view(&rec).await;
    assert!(
        active,
        "a capture is in flight, so the status leg must say so"
    );
    assert_eq!(filename, json!(started["filename"].as_str().unwrap()));

    let _ = rec.stop().await;
    path_restore(saved);

    // After the stop the recorder still holds `current_path` for its own reply,
    // but the status surface must not render a finished capture as if live.
    assert_eq!(recording_view(&rec).await, (false, Value::Null));
}

/// The stop reply carries the segment list and the interruption count.
#[tokio::test]
async fn gs_recording_stop_reports_segments_and_interruptions() {
    // The fake's own `sleep` resolves on PATH, which sibling tests mutate.
    let _guard = PATH_LOCK.lock().await;
    let bindir = tempfile::tempdir().unwrap();
    let fake = bindir.path().join("fake-ffmpeg");
    std::fs::write(&fake, "#!/bin/sh\nsleep 30\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let recdir = tempfile::tempdir().unwrap();
    let rec = GroundStationRecorder::new(recdir.path(), "rtsp://127.0.0.1:8554/main")
        .with_ffmpeg(fake.to_string_lossy().into_owned());
    let mut busy_retries = 0;
    let started = loop {
        match rec.start(Some("segments")).await {
            Ok(started) => break started,
            Err(e) if e.message.contains("Text file busy") && busy_retries < 20 => {
                busy_retries += 1;
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            Err(e) => panic!("start succeeds: {e:?}"),
        }
    };
    let body = body_json(json_ok(rec.stop().await.expect("stop succeeds"))).await;
    assert_eq!(body["segments"], json!([started["filename"]]));
    assert_eq!(body["interrupted_count"], json!(0));
    for key in ["filename", "stopped_at", "duration_seconds", "size_bytes"] {
        assert!(body.get(key).is_some(), "missing {key}");
    }
}

/// The two surfaces that report a live capture must agree, in BOTH states.
///
/// This is the regression that shipped: `/status` read the live recorder while
/// `/recording/list` hardcoded `recording: false`, so the cockpit's listing
/// panel denied the capture its own status panel was reporting. Pinning both
/// envelopes against ONE recorder is what stops a second derivation being
/// added back.
#[tokio::test]
async fn the_listing_and_status_envelopes_agree_on_the_recording_flag() {
    let _guard = PATH_LOCK.lock().await;
    let bindir = fake_ffmpeg_dir();
    let recdir = tempfile::tempdir().unwrap();
    let rec = GroundStationRecorder::new(recdir.path(), "rtsp://127.0.0.1:8554/main");

    // Idle: both surfaces say no capture, with no filename.
    let listing = crate::routes::gs_recording_list::recording_list_body(recdir.path(), &rec).await;
    let (status_active, status_filename) = recording_view(&rec).await;
    assert_eq!(listing["recording"], json!(false));
    assert_eq!(listing["recording"], json!(status_active));
    assert_eq!(listing["current_filename"], status_filename);
    assert_eq!(listing["current_filename"], Value::Null);

    // In flight: both surfaces say so, and name the same file.
    let saved = path_prepend(bindir.path());
    let started = rec
        .start(Some("bothsurfaces"))
        .await
        .expect("start succeeds");

    let listing = crate::routes::gs_recording_list::recording_list_body(recdir.path(), &rec).await;
    let (status_active, status_filename) = recording_view(&rec).await;
    assert_eq!(
        listing["recording"],
        json!(true),
        "a capture is in flight, so the LISTING envelope must say so too"
    );
    assert_eq!(listing["recording"], json!(status_active));
    assert_eq!(listing["current_filename"], status_filename);
    assert_eq!(
        listing["current_filename"],
        json!(started["filename"].as_str().unwrap())
    );

    let _ = rec.stop().await;
    path_restore(saved);

    // And after the stop both fall back together, rather than one of them
    // freezing on the finished capture.
    let listing = crate::routes::gs_recording_list::recording_list_body(recdir.path(), &rec).await;
    let (status_active, status_filename) = recording_view(&rec).await;
    assert_eq!(listing["recording"], json!(false));
    assert_eq!(listing["recording"], json!(status_active));
    assert_eq!(listing["current_filename"], status_filename);
}
