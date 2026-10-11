use super::capture::segment_path;
use super::*;
use std::path::Path;
use tokio::sync::Mutex;

/// Serializes the tests that mutate the process-global `PATH` so a parallel
/// test in this crate cannot clobber it mid-flight. Held for the whole body of
/// each PATH-mutating test. A `tokio` Mutex so the guard is safe to hold across
/// the `.await` points in the recorder start/stop calls.
static PATH_LOCK: Mutex<()> = Mutex::const_new(());

#[test]
fn filename_has_no_hint_when_absent() {
    let name = generate_filename(None);
    assert!(name.ends_with(".mp4"));
    // No underscore-separated hint segment.
    assert!(!name.trim_end_matches(".mp4").contains('_'));
}

#[test]
fn filename_appends_a_sanitised_hint() {
    let name = generate_filename(Some("test flight!@#"));
    assert!(name.ends_with("_testflight.mp4"), "got {name}");
}

#[test]
fn filename_keeps_dash_and_underscore_in_the_hint() {
    let name = generate_filename(Some("pipe-run_2"));
    assert!(name.ends_with("_pipe-run_2.mp4"), "got {name}");
}

#[test]
fn filename_drops_a_hint_that_sanitises_to_empty() {
    let name = generate_filename(Some("!!!"));
    assert!(name.ends_with(".mp4"));
    assert!(!name.trim_end_matches(".mp4").contains('_'));
}

#[test]
fn filename_truncates_a_long_hint_to_48_chars() {
    let long = "a".repeat(100);
    let name = generate_filename(Some(&long));
    let hint = name
        .trim_end_matches(".mp4")
        .rsplit('_')
        .next()
        .unwrap_or("");
    assert_eq!(hint.len(), 48);
}

#[test]
fn iso8601_renders_a_utc_offset_not_a_z() {
    let s = now_iso8601();
    assert!(s.ends_with("+00:00"), "got {s}");
    assert!(!s.ends_with('Z'));
}

#[test]
fn round2_rounds_to_two_decimals() {
    assert_eq!(round2(60.0), 60.0);
    assert_eq!(round2(1.2345), 1.23);
    assert_eq!(round2(1.235), 1.24);
}

#[tokio::test]
async fn an_idle_recorder_is_not_active() {
    let dir = tempfile::tempdir().unwrap();
    let rec = GroundStationRecorder::new(dir.path(), DEFAULT_RTSP_URL);
    assert!(!rec.is_active().await);
    assert_eq!(rec.current_filename().await, None);
}

#[tokio::test]
async fn stop_when_idle_is_not_active_error() {
    let dir = tempfile::tempdir().unwrap();
    let rec = GroundStationRecorder::new(dir.path(), DEFAULT_RTSP_URL);
    let err = rec.stop().await.unwrap_err();
    assert_eq!(err.code, "E_RECORDING_NOT_ACTIVE");
    assert_eq!(err.message, "no recording is currently active");
}

#[tokio::test]
async fn start_without_ffmpeg_is_ffmpeg_not_found() {
    let _guard = PATH_LOCK.lock().await;
    // Empty PATH → ffmpeg is absent → the 503 error code, before any spawn.
    let path_save = std::env::var("PATH").ok();
    let dir = tempfile::tempdir().unwrap();
    let rec = GroundStationRecorder::new(dir.path(), DEFAULT_RTSP_URL);
    // Set an empty PATH for this check; restore after.
    std::env::set_var("PATH", "");
    let err = rec.start(None).await.unwrap_err();
    if let Some(p) = path_save {
        std::env::set_var("PATH", p);
    } else {
        std::env::remove_var("PATH");
    }
    assert_eq!(err.code, "E_FFMPEG_NOT_FOUND");
}

#[tokio::test]
async fn start_and_stop_full_cycle_against_a_fake_ffmpeg() {
    let _guard = PATH_LOCK.lock().await;
    // Put a fake `ffmpeg` on the PATH that just sleeps, so start succeeds and
    // stop terminates it. The fake writes a few bytes to the output path it is
    // handed so the stop summary reports a real size.
    let bindir = tempfile::tempdir().unwrap();
    let fake = bindir.path().join("ffmpeg");
    std::fs::write(
            &fake,
            "#!/bin/sh\n# the last arg is the output path; write to it then sleep\nfor out; do :; done\nprintf 'data' > \"$out\"\nsleep 30\n",
        )
        .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let recdir = tempfile::tempdir().unwrap();
    let rec = GroundStationRecorder::new(recdir.path(), DEFAULT_RTSP_URL);

    let path_save = std::env::var("PATH").ok();
    // Prepend the fake dir so `ffmpeg` resolves to the fake, but keep the real
    // PATH so the fake's own `sh`/`printf`/`sleep` still resolve.
    let combined = match &path_save {
        Some(orig) => format!("{}:{}", bindir.path().display(), orig),
        None => bindir.path().display().to_string(),
    };
    std::env::set_var("PATH", &combined);

    let started = rec.start(Some("cycle")).await.expect("start succeeds");
    assert!(rec.is_active().await, "a capture is in flight after start");
    // The start body shape.
    let started_name = started["filename"].as_str().unwrap().to_string();
    assert!(started_name.ends_with("_cycle.mp4"));
    assert!(started["started_at"].as_str().unwrap().ends_with("+00:00"));
    assert!(started["path"]
        .as_str()
        .unwrap()
        .ends_with(&format!("/{started_name}")));

    // A second start while active is the 409.
    let busy = rec.start(None).await.unwrap_err();
    assert_eq!(busy.code, "E_RECORDING_ACTIVE");

    // Give the fake a moment to write the output file.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let stopped = rec.stop().await.expect("stop succeeds");
    if let Some(p) = path_save {
        std::env::set_var("PATH", p);
    } else {
        std::env::remove_var("PATH");
    }
    assert!(!rec.is_active().await, "idle after stop");
    // The stop body shape: the four fields, the filename echoed.
    assert_eq!(stopped["filename"].as_str().unwrap(), started_name);
    assert!(stopped["stopped_at"].as_str().unwrap().ends_with("+00:00"));
    assert!(stopped["duration_seconds"].as_f64().unwrap() >= 0.0);
    assert_eq!(stopped["size_bytes"].as_u64().unwrap(), 4); // "data"
}

/// A capture whose ffmpeg is chatty on stderr must keep writing.
///
/// The fake writes far more than a pipe buffer holds to stderr BEFORE it
/// touches the output file. With stderr piped and never read, the write
/// blocks once the pipe fills and the output never appears — exactly the
/// recording that silently stops growing mid-flight.
#[tokio::test]
async fn a_chatty_ffmpeg_keeps_recording() {
    let _guard = PATH_LOCK.lock().await;
    let bindir = tempfile::tempdir().unwrap();
    let fake = bindir.path().join("ffmpeg");
    std::fs::write(
            &fake,
            "#!/bin/sh\nfor out; do :; done\n\
             i=0; while [ $i -lt 4000 ]; do echo \"frame=$i fps=30 q=23.0 size=512kB time=00:00:01 bitrate=4000kbits/s speed=1x\" >&2; i=$((i+1)); done\n\
             printf 'data' > \"$out\"\nsleep 30\n",
        )
        .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let recdir = tempfile::tempdir().unwrap();
    let rec = GroundStationRecorder::new(recdir.path(), DEFAULT_RTSP_URL);
    let path_save = std::env::var("PATH").ok();
    let combined = match &path_save {
        Some(orig) => format!("{}:{}", bindir.path().display(), orig),
        None => bindir.path().display().to_string(),
    };
    std::env::set_var("PATH", &combined);

    let started = rec.start(Some("chatty")).await.expect("start succeeds");
    let out_path = std::path::PathBuf::from(started["path"].as_str().unwrap());
    let mut written = false;
    for _ in 0..100 {
        if std::fs::read(&out_path).is_ok_and(|b| b == b"data") {
            written = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let _ = rec.stop().await;
    if let Some(p) = path_save {
        std::env::set_var("PATH", p);
    } else {
        std::env::remove_var("PATH");
    }
    assert!(
        written,
        "the capture blocked on a full stderr pipe and never wrote its output"
    );
}

/// The recorder's muxer flags are a data-integrity contract, not tuning.
///
/// A plain or `+faststart` MP4 writes its `moov` atom only when the muxer
/// closes cleanly, so a ground station that loses power, is SIGKILLed or
/// fills its disk mid-capture leaves a file `ffprobe` reports as "moov
/// atom not found" — the whole flight recording, not its tail. This test
/// captures the literal argv the recorder hands ffmpeg so a future
/// simplification of the invocation cannot silently give that back.
#[tokio::test]
async fn the_capture_is_muxed_as_a_fragmented_mp4() {
    let _guard = PATH_LOCK.lock().await;
    let bindir = tempfile::tempdir().unwrap();
    let fake = bindir.path().join("ffmpeg");
    // Record the argv beside the output file, then idle so the capture
    // stays "in flight" while the assertions run.
    std::fs::write(
        &fake,
        "#!/bin/sh\nfor out; do :; done\nprintf '%s\\n' \"$@\" > \"$out.argv\"\nsleep 30\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let recdir = tempfile::tempdir().unwrap();
    let rec = GroundStationRecorder::new(recdir.path(), DEFAULT_RTSP_URL);

    let path_save = std::env::var("PATH").ok();
    let combined = match &path_save {
        Some(orig) => format!("{}:{}", bindir.path().display(), orig),
        None => bindir.path().display().to_string(),
    };
    std::env::set_var("PATH", &combined);

    // A script written moments ago can refuse to exec with ETXTBSY while a
    // fork running concurrently in another test still holds the write
    // descriptor it inherited. That is a test-timing artefact, not the
    // recorder's behaviour, so retry it briefly.
    let mut busy_retries = 0;
    let started = loop {
        match rec.start(Some("frag")).await {
            Ok(started) => break started,
            Err(e) if e.message.contains("Text file busy") && busy_retries < 20 => {
                busy_retries += 1;
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            Err(e) => panic!("start succeeds: {e:?}"),
        }
    };
    let out_path = started["path"].as_str().unwrap().to_string();
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let _ = rec.stop().await;
    if let Some(p) = path_save {
        std::env::set_var("PATH", p);
    } else {
        std::env::remove_var("PATH");
    }

    let argv: Vec<String> = std::fs::read_to_string(format!("{out_path}.argv"))
        .expect("the fake ffmpeg recorded its argv")
        .lines()
        .map(str::to_string)
        .collect();

    let movflags = argv
        .iter()
        .position(|a| a == "-movflags")
        .expect("the recorder passes -movflags");
    assert_eq!(argv[movflags + 1], RECORDER_MOVFLAGS);
    assert!(
        argv[movflags + 1].contains("frag_keyframe")
            && argv[movflags + 1].contains("empty_moov")
            && argv[movflags + 1].contains("default_base_moof"),
        "a capture killed mid-write is only playable with all three fMP4 flags: {argv:?}"
    );
    assert!(
        !argv.iter().any(|a| a.contains("faststart")),
        "+faststart defers the moov atom to a clean exit: {argv:?}"
    );
    let flush = argv
        .iter()
        .position(|a| a == "-flush_packets")
        .expect("the recorder flushes each packet out of the AVIO buffer");
    assert_eq!(argv[flush + 1], "1");
}

#[test]
fn segments_are_numbered_beside_the_first_file() {
    let first = Path::new("/rec/2026-01-01T00-00-00_flight.mp4");
    assert_eq!(
        segment_path(first, 2),
        Path::new("/rec/2026-01-01T00-00-00_flight_part2.mp4")
    );
}

/// A capture whose ffmpeg exits while the recording is wanted continues in
/// a new segment, and stop reports every segment and the interruption.
#[tokio::test]
async fn an_interrupted_capture_continues_in_a_new_segment() {
    // The fake's own `sleep` resolves on PATH, which sibling tests mutate.
    let _guard = PATH_LOCK.lock().await;
    let bindir = tempfile::tempdir().unwrap();
    let fake = bindir.path().join("fake-ffmpeg");
    // The first segment ends at once (an RTSP EOF); later ones keep going.
    std::fs::write(
        &fake,
        "#!/bin/sh\nfor out; do :; done\nprintf 'data' > \"$out\"\n\
             case \"$out\" in *_part*) sleep 30 ;; *) exit 1 ;; esac\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let recdir = tempfile::tempdir().unwrap();
    let rec = GroundStationRecorder::new(recdir.path(), DEFAULT_RTSP_URL)
        .with_ffmpeg(fake.to_string_lossy().into_owned());

    let mut busy_retries = 0;
    let started = loop {
        match rec.start(Some("seg")).await {
            Ok(started) => break started,
            Err(e) if e.message.contains("Text file busy") && busy_retries < 20 => {
                busy_retries += 1;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => panic!("start succeeds: {e:?}"),
        }
    };
    let first = started["filename"].as_str().unwrap().to_string();
    let second = format!("{}_part2.mp4", first.trim_end_matches(".mp4"));
    let second_path = recdir.path().join(&second);
    let mut seen = false;
    for _ in 0..80 {
        if second_path.exists() {
            seen = true;
            break;
        }
        // Still wanted between segments.
        assert!(rec.is_active().await);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(seen, "a new segment starts after the capture exits");

    let stopped = rec.stop().await.expect("stop succeeds");
    assert_eq!(stopped["filename"].as_str().unwrap(), first);
    assert_eq!(stopped["segments"], json!([first, second]));
    assert_eq!(stopped["interrupted_count"], 1);
    assert_eq!(stopped["size_bytes"].as_u64().unwrap(), 8);
    assert!(!rec.is_active().await);
}
