//! **`GET /api/video/snapshot`**: one still frame from the live primary stream.
//!
//! Grabs a single frame on demand from the local mediamtx RTSP leg of the `main`
//! path with a one-shot `ffmpeg`, and answers it as `image/jpeg`. Nothing is
//! cached or stored, so the frame is never older than the request: the
//! `X-Captured-At` header (RFC 3339, UTC) carries the moment `ffmpeg` handed it
//! back, and `Cache-Control: no-store` keeps a browser from replaying it.
//!
//! When no frame can be had (no publisher on `main`, `ffmpeg` missing or failing,
//! or the grab overrunning its deadline) the route answers `503
//! {"error":"E_NO_FRAME","message":...}` rather than an old picture.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use ados_video::mediamtx::DEFAULT_RTSP_PORT;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// The grab deadline. An RTSP connect plus the wait for the next keyframe fits
/// well inside it on a live stream; past it the stream is not delivering.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(5);

/// The frame grabber, resolved on `PATH`.
const FFMPEG: &str = "ffmpeg";

/// `GET /api/video/snapshot` → `200 image/jpeg` with `X-Captured-At`, or `503
/// E_NO_FRAME`.
pub async fn get_video_snapshot() -> Response {
    let url = format!("rtsp://127.0.0.1:{DEFAULT_RTSP_PORT}/main");
    capture_snapshot(Path::new(FFMPEG), &url, SNAPSHOT_TIMEOUT).await
}

/// Grab one frame from `rtsp_url` with `program` (an `ffmpeg`) inside `deadline`.
/// The program is a parameter so a test can stand a fake grabber in for it.
async fn capture_snapshot(program: &Path, rtsp_url: &str, deadline: Duration) -> Response {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-rtsp_transport",
        "tcp",
        "-i",
        rtsp_url,
        "-frames:v",
        "1",
        "-c:v",
        "mjpeg",
        "-f",
        "image2",
        "-",
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    // A grab that overruns the deadline is dropped with its future; this kills
    // the child with it instead of leaving it attached to the stream.
    .kill_on_drop(true);

    let output = match tokio::time::timeout(deadline, cmd.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => {
            return no_frame(&format!("the frame grabber could not run: {e}"));
        }
        Err(_) => return no_frame("no frame arrived from the primary stream in time"),
    };
    let captured_at = OffsetDateTime::now_utc();

    if !output.status.success() {
        return no_frame("the primary stream did not deliver a frame");
    }
    // A JPEG starts with the SOI marker; anything else is not a still.
    if !output.stdout.starts_with(&[0xFF, 0xD8]) {
        return no_frame("the frame grabber returned no image");
    }

    let stamp = captured_at
        .format(&Rfc3339)
        .ok()
        .and_then(|s| HeaderValue::from_str(&s).ok());
    let mut resp = (StatusCode::OK, output.stdout).into_response();
    let headers = resp.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/jpeg"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Some(stamp) = stamp {
        headers.insert("x-captured-at", stamp);
    }
    resp
}

/// The `503 {"error":"E_NO_FRAME","message":...}` answer.
fn no_frame(message: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"error": "E_NO_FRAME", "message": message})),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "rtsp://127.0.0.1:1/main";

    async fn json_body(resp: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Write an executable shell script standing in for `ffmpeg`. Each script
    /// gets its own name so no test rewrites a file another one is executing.
    fn fake_grabber(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[tokio::test]
    async fn a_missing_grabber_is_a_503_no_frame() {
        let dir = tempfile::tempdir().unwrap();
        let resp = capture_snapshot(&dir.path().join("absent"), URL, SNAPSHOT_TIMEOUT).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(json_body(resp).await["error"], "E_NO_FRAME");
    }

    #[tokio::test]
    async fn a_failing_or_silent_grabber_is_a_503_no_frame() {
        let dir = tempfile::tempdir().unwrap();
        for (i, body) in ["exit 1", "exit 0", "sleep 30"].into_iter().enumerate() {
            let grabber = fake_grabber(dir.path(), &format!("ffmpeg-{i}"), body);
            let resp = capture_snapshot(&grabber, URL, Duration::from_millis(500)).await;
            assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE, "{body}");
            assert_eq!(json_body(resp).await["error"], "E_NO_FRAME", "{body}");
        }
    }

    #[tokio::test]
    async fn a_delivered_frame_is_a_jpeg_stamped_with_its_capture_time() {
        let dir = tempfile::tempdir().unwrap();
        let grabber = fake_grabber(
            dir.path(),
            "ffmpeg",
            r"printf '\377\330\377\340JFIF\377\331'",
        );
        let before = OffsetDateTime::now_utc();
        let resp = capture_snapshot(&grabber, URL, SNAPSHOT_TIMEOUT).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()[header::CONTENT_TYPE], "image/jpeg");
        assert_eq!(resp.headers()[header::CACHE_CONTROL], "no-store");
        let stamp = resp.headers()["x-captured-at"]
            .to_str()
            .unwrap()
            .to_string();
        let captured = OffsetDateTime::parse(&stamp, &Rfc3339).expect("RFC 3339 stamp");
        // RFC 3339 here carries sub-second precision, so the stamp is not before
        // the request started.
        assert!(captured >= before, "{stamp} predates the request");
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(bytes.starts_with(&[0xFF, 0xD8]));
    }
}
