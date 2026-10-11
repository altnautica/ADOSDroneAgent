use super::*;
use std::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Read a response body as JSON.
async fn body_json(resp: Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// Read a response body as bytes.
async fn body_bytes(resp: Response) -> Vec<u8> {
    axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec()
}

#[tokio::test]
async fn profile_mismatch_body_is_the_fastapi_404_shape() {
    let resp = profile_mismatch();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        body_json(resp).await,
        json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})
    );
}

// --- the listing --------------------------------------------------------

#[tokio::test]
async fn list_of_an_absent_dir_is_empty() {
    // A fresh ground station that has never recorded has no recordings dir;
    // the list is empty (never an error), matching the Python `is_dir()` guard.
    let dir = tempfile::tempdir().unwrap();
    let absent = dir.path().join("never-created");
    assert_eq!(list_recordings(&absent).await, Vec::<Value>::new());
}

#[tokio::test]
async fn list_of_an_empty_dir_is_empty() {
    // An existing-but-empty recordings dir also yields the empty list.
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(list_recordings(dir.path()).await, Vec::<Value>::new());
}

#[tokio::test]
async fn list_enumerates_mp4_files_with_the_three_fields() {
    // Two .mp4 files land in the list, each with filename + size_bytes + mtime;
    // a non-.mp4 file and a subdirectory are both excluded.
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.mp4"), b"aaaa").unwrap();
    fs::write(dir.path().join("b.mp4"), b"bbbbbbbb").unwrap();
    fs::write(dir.path().join("notes.txt"), b"x").unwrap();
    fs::create_dir(dir.path().join("subdir.mp4")).unwrap();

    let items = list_recordings(dir.path()).await;
    assert_eq!(items.len(), 2);
    for item in &items {
        let obj = item.as_object().unwrap();
        // Exactly the three contract fields, no more.
        assert_eq!(obj.len(), 3);
        assert!(obj.get("filename").and_then(Value::as_str).is_some());
        assert!(obj.get("size_bytes").and_then(Value::as_i64).is_some());
        assert!(obj.get("mtime").and_then(Value::as_f64).is_some());
    }
    let names: Vec<&str> = items
        .iter()
        .filter_map(|i| i.get("filename").and_then(Value::as_str))
        .collect();
    assert!(names.contains(&"a.mp4"));
    assert!(names.contains(&"b.mp4"));
    assert!(!names.contains(&"notes.txt"));
    assert!(!names.contains(&"subdir.mp4"));
}

#[tokio::test]
async fn list_reports_size_bytes_from_the_file() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("clip.mp4"), b"0123456789").unwrap();
    let items = list_recordings(dir.path()).await;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["filename"], json!("clip.mp4"));
    assert_eq!(items[0]["size_bytes"], json!(10));
}

#[tokio::test]
async fn list_is_newest_first_by_mtime() {
    // Two files with explicit, distinct mtimes must come back newest-first.
    use std::time::{Duration as StdDuration, SystemTime};
    let dir = tempfile::tempdir().unwrap();
    let old = dir.path().join("old.mp4");
    let new = dir.path().join("new.mp4");
    fs::write(&old, b"o").unwrap();
    fs::write(&new, b"n").unwrap();

    let base = SystemTime::now();
    filetime_set(&old, base - StdDuration::from_secs(100));
    filetime_set(&new, base);

    let items = list_recordings(dir.path()).await;
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["filename"], json!("new.mp4"));
    assert_eq!(items[1]["filename"], json!("old.mp4"));
}

#[tokio::test]
async fn mp4_suffix_match_is_case_insensitive() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("upper.MP4"), b"x").unwrap();
    fs::write(dir.path().join("mixed.Mp4"), b"x").unwrap();
    assert_eq!(list_recordings(dir.path()).await.len(), 2);
}

#[tokio::test]
async fn a_symlink_named_mp4_is_not_a_recording_and_does_not_break_the_scan() {
    // The extension matches on both links, but neither is a file this
    // directory holds: `DirEntry::metadata` is `symlink_metadata` on Unix (in
    // `tokio::fs` exactly as in `std::fs`), so a link reads as a link and is
    // filtered, and the real recording beside it is still listed. A dangling
    // link is the case that could have thrown instead of skipping.
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("real.mp4"), b"rrrr").unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(dir.path().join("gone"), dir.path().join("dangling.mp4"))
            .unwrap();
        std::os::unix::fs::symlink(dir.path().join("real.mp4"), dir.path().join("alias.mp4"))
            .unwrap();
    }

    let items = list_recordings(dir.path()).await;
    let names: Vec<&str> = items
        .iter()
        .filter_map(|i| i.get("filename").and_then(Value::as_str))
        .collect();
    assert_eq!(names, vec!["real.mp4"]);
}

/// Set a file's mtime directly so the newest-first ordering test is
/// deterministic, without depending on filesystem write-order timing.
fn filetime_set(path: &Path, when: std::time::SystemTime) {
    // `File::set_modified` sets the mtime directly (no extra dependency).
    let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    f.set_modified(when).unwrap();
}

// --- the stream-path + clip parameter gates -----------------------------

#[test]
fn stream_path_allow_list_refuses_separators_and_parent_hops() {
    assert!(is_valid_stream_path("main"));
    assert!(is_valid_stream_path("cam-1_sub.h264"));
    // Empty, traversal, separators, and anything that could break a request
    // line are all refused.
    assert!(!is_valid_stream_path(""));
    assert!(!is_valid_stream_path(".."));
    assert!(!is_valid_stream_path("../../etc/ados"));
    assert!(!is_valid_stream_path("a/b"));
    assert!(!is_valid_stream_path("a b"));
    assert!(!is_valid_stream_path("main\r\nX-Evil: 1"));
    assert!(!is_valid_stream_path(&"m".repeat(MAX_STREAM_PATH + 1)));
}

#[test]
fn an_unparseable_start_is_refused_before_the_proxy() {
    let query = ClipQuery {
        path: Some("main".to_string()),
        start: Some("yesterday afternoon".to_string()),
        duration: Some("10".to_string()),
    };
    let reject = validated_clip(&query).expect_err("an unparseable start is refused");
    assert_eq!(reject.code, "E_INVALID_START");
}

#[test]
fn a_non_positive_or_absurd_duration_is_refused() {
    for bad in ["0", "-5", "36000", "nan", "inf", "soon", ""] {
        let query = ClipQuery {
            path: Some("main".to_string()),
            start: Some("2026-09-03T12:00:00Z".to_string()),
            duration: Some(bad.to_string()),
        };
        let reject =
            validated_clip(&query).expect_err(&format!("duration {bad:?} must be refused"));
        assert_eq!(
            reject.code, "E_INVALID_DURATION",
            "duration {bad:?} must be refused as a duration"
        );
    }
}

#[test]
fn a_stream_path_outside_the_allow_list_is_refused_on_the_clip_route_too() {
    let query = ClipQuery {
        path: Some("../../etc/ados".to_string()),
        start: Some("2026-09-03T12:00:00Z".to_string()),
        duration: Some("10".to_string()),
    };
    let reject = validated_clip(&query).expect_err("a traversal path is refused");
    assert_eq!(reject.code, "E_INVALID_PATH");
}

#[test]
fn a_valid_clip_query_forwards_its_three_parameters() {
    let query = ClipQuery {
        path: Some("main".to_string()),
        start: Some("2026-09-03T12:00:00Z".to_string()),
        duration: Some("30.5".to_string()),
    };
    let (path, start, duration) = validated_clip(&query).expect("valid");
    assert_eq!(path, "main");
    assert_eq!(start, "2026-09-03T12:00:00Z");
    assert!((duration - 30.5).abs() < f64::EPSILON);
}

#[test]
fn a_timestamp_offset_is_percent_encoded_not_turned_into_a_space() {
    // A raw `+` in a query decodes as a space, so a non-UTC offset must be
    // encoded or mediamtx receives a timestamp it cannot parse.
    assert_eq!(
        encode_query_value("2026-09-03T12:00:00+05:30"),
        "2026-09-03T12%3A00%3A00%2B05%3A30"
    );
}

// --- the playback proxy -------------------------------------------------

/// Serve exactly one canned HTTP response on an ephemeral loopback port,
/// standing in for the mediamtx playback server.
async fn serve_once(response: &'static str) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        if let Ok((mut stream, _)) = listener.accept().await {
            // Drain the request first. Writing before reading would leave the
            // client's request bytes arriving at a socket this task then
            // closes, which the OS answers with an RST that discards the
            // response we just wrote.
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).await;
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.flush().await;
        }
    });
    port
}

#[tokio::test]
async fn the_playback_proxy_relays_the_upstream_status_and_body() {
    let port = serve_once(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 31\r\n\r\n\
             [{\"start\":\"x\",\"duration\":60.0}]",
    )
    .await;
    let upstream = playback_get(port, "/list?path=main")
        .await
        .expect("the loopback playback server answered");
    let relayed = crate::proxy::relay_response(upstream);
    assert_eq!(relayed.status(), StatusCode::OK);
    assert_eq!(
        body_json(relayed).await,
        json!([{"start": "x", "duration": 60.0}])
    );
}

#[tokio::test]
async fn the_playback_proxy_relays_a_binary_clip_body() {
    // An fMP4 clip is not JSON; the relay must pass the bytes and the content
    // type through untouched so a browser `<video>` can play them.
    let port =
        serve_once("HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\nContent-Length: 4\r\n\r\nftyp")
            .await;
    let upstream = playback_get(port, "/get?path=main&start=x&duration=1")
        .await
        .expect("the loopback playback server answered");
    let relayed = crate::proxy::relay_response(upstream);
    assert_eq!(
        relayed
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("video/mp4")
    );
    assert_eq!(body_bytes(relayed).await, b"ftyp".to_vec());
}

#[tokio::test]
async fn a_closed_playback_port_is_a_503_not_a_hang() {
    // Bind an ephemeral port and drop the listener so the connect is refused.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    assert!(playback_get(port, "/list?path=main").await.is_none());
    let resp = playback_unavailable();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        body_json(resp).await["detail"]["error"]["code"],
        json!("E_PLAYBACK_UNAVAILABLE")
    );
}

// --- the DELETE traversal gate -----------------------------------------

#[test]
fn a_segment_name_with_a_separator_or_parent_hop_is_not_a_plain_name() {
    assert!(is_plain_segment_name("2026-09-03_12-00-00-000000.mp4"));
    assert!(!is_plain_segment_name(""));
    assert!(!is_plain_segment_name("."));
    assert!(!is_plain_segment_name(".."));
    assert!(!is_plain_segment_name("../config.yaml"));
    assert!(!is_plain_segment_name("../../etc/ados/config.yaml"));
    assert!(!is_plain_segment_name("main/seg.mp4"));
    assert!(!is_plain_segment_name("/etc/ados/config.yaml"));
    assert!(!is_plain_segment_name("a\\b"));
    assert!(!is_plain_segment_name("seg..mp4"));
    assert!(!is_plain_segment_name(&"x".repeat(MAX_SEGMENT_NAME + 1)));
}

#[tokio::test]
async fn a_traversal_attempt_is_refused_and_leaves_the_target_alone() {
    // The hazard: the DELETE takes its target from the URL and axum
    // percent-decodes it, so `%2e%2e%2f` arrives as `../`. The file outside
    // the recordings directory must survive.
    let root = tempfile::tempdir().unwrap();
    let recordings = root.path().join("recordings");
    fs::create_dir(&recordings).unwrap();
    let outsider = root.path().join("config.yaml");
    fs::write(&outsider, b"agent:\n  name: x\n").unwrap();

    for attempt in ["../config.yaml", "../../etc/ados/config.yaml", ".."] {
        assert_eq!(
            resolve_segment(&recordings, attempt).await,
            Err(SegmentReject::Invalid),
            "{attempt} must be refused as a name, never resolved"
        );
    }
    assert!(outsider.exists(), "the traversal target must be untouched");
}

#[tokio::test]
async fn a_symlink_pointing_out_of_the_recordings_dir_is_refused() {
    // The case a name check cannot see: a plain, separator-free name whose
    // resolved path is outside the directory.
    let root = tempfile::tempdir().unwrap();
    let recordings = root.path().join("recordings");
    fs::create_dir(&recordings).unwrap();
    let outsider = root.path().join("config.yaml");
    fs::write(&outsider, b"agent:\n  name: x\n").unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outsider, recordings.join("escape.mp4")).unwrap();
        assert_eq!(
            resolve_segment(&recordings, "escape.mp4").await,
            Err(SegmentReject::Invalid),
            "a link out of the recordings dir must be refused"
        );
        assert!(outsider.exists(), "the link target must be untouched");
    }
}

#[tokio::test]
async fn a_real_segment_resolves_and_a_directory_does_not() {
    let recordings = tempfile::tempdir().unwrap();
    fs::write(recordings.path().join("seg.mp4"), b"data").unwrap();
    fs::create_dir(recordings.path().join("main")).unwrap();

    let resolved = resolve_segment(recordings.path(), "seg.mp4")
        .await
        .expect("a real segment resolves");
    assert!(resolved.ends_with("seg.mp4"));
    // A directory is not a segment: a DELETE here takes one file, never a tree.
    assert_eq!(
        resolve_segment(recordings.path(), "main").await,
        Err(SegmentReject::Absent)
    );
    assert_eq!(
        resolve_segment(recordings.path(), "nope.mp4").await,
        Err(SegmentReject::Absent)
    );
}
