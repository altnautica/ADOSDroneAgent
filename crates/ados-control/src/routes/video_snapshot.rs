//! **`GET /api/video/snapshot`**: one still frame from the live primary stream.
//!
//! Grabs a single frame on demand from the local mediamtx RTSP leg of the `main`
//! path with a one-shot `ffmpeg`, and answers it as `image/jpeg`. Nothing is
//! cached or stored, so the frame is never older than the request: the
//! `X-Captured-At` header (RFC 3339, UTC) carries the moment the frame arrived
//! from the stream (see [`capture_time`]), and `Cache-Control: no-store` keeps a
//! browser from replaying it.
//!
//! ## Why the deadline follows the stream rate
//!
//! A drone on the radio link boots its encoder on the 1 fps thumbnail profile.
//! A stock `ffmpeg -i rtsp://…` cannot hand back a frame of such a stream inside
//! a fixed 5 s: it probes about five seconds of stream before it opens the
//! input, and its frame-threaded H.264 decoder holds one frame per thread before
//! it emits anything. So the grab disables both (`-probesize 32
//! -analyzeduration 0`, `-threads 1`). What is left is fixed by the stream
//! itself: the wait for the next frame after PLAY, plus one more frame period,
//! because the demuxer's parser closes an access unit only when the next one
//! begins. The deadline is therefore sized from the live encoder rate the video
//! profile sidecar publishes ([`snapshot_deadline`]), never below the floor a
//! full-rate stream is held to.
//!
//! When no frame can be had (no publisher on `main`, `ffmpeg` missing or failing,
//! or the grab overrunning its deadline) the route answers `503
//! {"error":"E_NO_FRAME","message":...}` rather than an old picture.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use ados_video::mediamtx::DEFAULT_RTSP_PORT;
use ados_video::profile::{EncoderState, VIDEO_PROFILE_SIDECAR};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// The deadline a full-rate stream is held to. An RTSP connect plus the wait for
/// the next keyframe fits well inside it; past it the stream is not delivering.
const SNAPSHOT_FLOOR: Duration = Duration::from_secs(5);

/// The grab's own cost, independent of the stream rate: spawning `ffmpeg`, the
/// RTSP DESCRIBE/SETUP/PLAY handshake, one decode and one JPEG encode, with
/// margin for a loaded single-board computer.
const GRAB_OVERHEAD: Duration = Duration::from_secs(3);

/// Frame periods a grab can spend waiting on the stream: up to one for the next
/// frame after PLAY, one for the parser to close it, and one of margin.
const FRAME_PERIODS_PER_GRAB: u32 = 3;

/// The rate assumed when the encoder has not published one: the slowest profile
/// the encoder runs (the 1 fps thumbnail), so an unknown rate never shortens the
/// deadline below what the live stream needs.
const SLOWEST_STREAM_FPS: u32 = 1;

/// The frame grabber, resolved on `PATH`.
const FFMPEG: &str = "ffmpeg";

/// `GET /api/video/snapshot` → `200 image/jpeg` with `X-Captured-At`, or `503
/// E_NO_FRAME`.
pub async fn get_video_snapshot() -> Response {
    let url = format!("rtsp://127.0.0.1:{DEFAULT_RTSP_PORT}/main");
    let fps = live_stream_fps(Path::new(VIDEO_PROFILE_SIDECAR)).await;
    capture_snapshot(Path::new(FFMPEG), &url, fps, snapshot_deadline(fps)).await
}

/// The encoder's live frame rate from the video profile sidecar, or the slowest
/// profile's rate when the sidecar is absent, unreadable or reports zero.
async fn live_stream_fps(sidecar: &Path) -> u32 {
    tokio::fs::read_to_string(sidecar)
        .await
        .ok()
        .and_then(|text| serde_json::from_str::<EncoderState>(&text).ok())
        .map(|state| state.fps)
        .filter(|fps| *fps > 0)
        .unwrap_or(SLOWEST_STREAM_FPS)
}

/// One frame period of a `fps` stream.
fn frame_period(fps: u32) -> Duration {
    Duration::from_secs(1) / fps.max(1)
}

/// How long a grab of a `fps` stream may take before the stream is judged not
/// to be delivering: the grab's own cost plus [`FRAME_PERIODS_PER_GRAB`] frame
/// periods, never below [`SNAPSHOT_FLOOR`].
fn snapshot_deadline(fps: u32) -> Duration {
    (GRAB_OVERHEAD + frame_period(fps) * FRAME_PERIODS_PER_GRAB).max(SNAPSHOT_FLOOR)
}

/// When the frame handed back at `handed_back` arrived from the stream.
///
/// The demuxer releases a frame only once the next one starts arriving, so the
/// frame came in one frame period before `ffmpeg` handed it back. The grab only
/// ever sees frames sent after its own PLAY, so the arrival is never before
/// `requested`; that bound also holds the stamp honest when the published rate
/// is higher than the stream's real one. The stamp is therefore never later
/// than the frame's arrival by more than the decode time, and never claims a
/// frame fresher than it is.
fn capture_time(
    requested: OffsetDateTime,
    handed_back: OffsetDateTime,
    fps: u32,
) -> OffsetDateTime {
    (handed_back - frame_period(fps)).max(requested)
}

/// Grab one frame from `rtsp_url` with `program` (an `ffmpeg`) inside `deadline`.
/// `fps` is the stream's live rate, used to date the frame. The program is a
/// parameter so a test can stand a fake grabber in for it.
async fn capture_snapshot(
    program: &Path,
    rtsp_url: &str,
    fps: u32,
    deadline: Duration,
) -> Response {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "error",
        // Open the input on the first packet instead of probing seconds of a
        // slow stream (`-analyzeduration 0` alone falls back to the 5 s default).
        "-probesize",
        "32",
        "-analyzeduration",
        "0",
        // One decoder thread: frame threading holds a frame per thread before
        // the first one comes out, which on a 1 fps stream is seconds.
        "-threads",
        "1",
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

    let requested = OffsetDateTime::now_utc();
    let output = match tokio::time::timeout(deadline, cmd.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => {
            return no_frame(&format!("the frame grabber could not run: {e}"));
        }
        Err(_) => return no_frame("no frame arrived from the primary stream in time"),
    };
    let captured_at = capture_time(requested, OffsetDateTime::now_utc(), fps);

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
        let resp = capture_snapshot(&dir.path().join("absent"), URL, 30, SNAPSHOT_FLOOR).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(json_body(resp).await["error"], "E_NO_FRAME");
    }

    #[tokio::test]
    async fn a_failing_or_silent_grabber_is_a_503_no_frame() {
        let dir = tempfile::tempdir().unwrap();
        for (i, body) in ["exit 1", "exit 0", "sleep 30"].into_iter().enumerate() {
            let grabber = fake_grabber(dir.path(), &format!("ffmpeg-{i}"), body);
            let resp = capture_snapshot(&grabber, URL, 30, Duration::from_millis(500)).await;
            assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE, "{body}");
            assert_eq!(json_body(resp).await["error"], "E_NO_FRAME", "{body}");
        }
    }

    #[tokio::test]
    async fn a_delivered_frame_is_a_jpeg_stamped_inside_the_grab() {
        let dir = tempfile::tempdir().unwrap();
        let grabber = fake_grabber(
            dir.path(),
            "ffmpeg",
            r"printf '\377\330\377\340JFIF\377\331'",
        );
        let before = OffsetDateTime::now_utc();
        let resp = capture_snapshot(&grabber, URL, 1, SNAPSHOT_FLOOR).await;
        let after = OffsetDateTime::now_utc();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()[header::CONTENT_TYPE], "image/jpeg");
        assert_eq!(resp.headers()[header::CACHE_CONTROL], "no-store");
        let stamp = resp.headers()["x-captured-at"]
            .to_str()
            .unwrap()
            .to_string();
        let captured = OffsetDateTime::parse(&stamp, &Rfc3339).expect("RFC 3339 stamp");
        // RFC 3339 here carries sub-second precision. The frame cannot predate
        // the request, and cannot be later than the grab's end.
        assert!(captured >= before, "{stamp} predates the request");
        assert!(captured <= after, "{stamp} postdates the grab");
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(bytes.starts_with(&[0xFF, 0xD8]));
    }

    #[test]
    fn a_one_fps_stream_is_given_time_for_three_frame_periods() {
        // A grab of the 1 fps thumbnail stream measured about 3 s with the
        // probe-free flags: a fixed 5 s held it with no margin on a loaded board.
        assert_eq!(snapshot_deadline(1), Duration::from_secs(6));
        assert!(snapshot_deadline(1) > SNAPSHOT_FLOOR);
        // A full-rate stream keeps the floor; the floor never shrinks.
        assert_eq!(snapshot_deadline(30), SNAPSHOT_FLOOR);
        assert_eq!(snapshot_deadline(10), SNAPSHOT_FLOOR);
        // A zero rate is read as the slowest one, not a divide-by-zero.
        assert_eq!(snapshot_deadline(0), snapshot_deadline(1));
    }

    #[test]
    fn the_capture_stamp_is_one_frame_before_the_hand_back_and_never_before_the_request() {
        let requested = OffsetDateTime::UNIX_EPOCH + Duration::from_secs(1_000);
        // A 1 fps grab that took 2.5 s: the frame arrived one period earlier.
        let handed_back = requested + Duration::from_millis(2_500);
        assert_eq!(
            capture_time(requested, handed_back, 1),
            requested + Duration::from_millis(1_500)
        );
        // A grab faster than one period of the published rate (the rate is
        // stale or wrong): the frame still came after PLAY, so the request bounds it.
        let quick = requested + Duration::from_millis(300);
        assert_eq!(capture_time(requested, quick, 1), requested);
        // A 25 fps stream: 40 ms before the hand-back.
        assert_eq!(
            capture_time(requested, handed_back, 25),
            handed_back - Duration::from_millis(40)
        );
    }

    #[tokio::test]
    async fn the_stream_rate_comes_from_the_profile_sidecar_and_defaults_to_the_slowest() {
        let dir = tempfile::tempdir().unwrap();
        let sidecar = dir.path().join("video-profile.json");
        let write = |fps: u32| {
            std::fs::write(
                &sidecar,
                json!({
                    "profile": "thumbnail", "ceiling_kbps": null, "width": 320,
                    "height": 180, "fps": fps, "bitrate_kbps": 50,
                })
                .to_string(),
            )
            .unwrap();
        };
        assert_eq!(live_stream_fps(&sidecar).await, SLOWEST_STREAM_FPS);
        write(30);
        assert_eq!(live_stream_fps(&sidecar).await, 30);
        write(1);
        assert_eq!(live_stream_fps(&sidecar).await, 1);
        write(0);
        assert_eq!(live_stream_fps(&sidecar).await, SLOWEST_STREAM_FPS);
        std::fs::write(&sidecar, "not json").unwrap();
        assert_eq!(live_stream_fps(&sidecar).await, SLOWEST_STREAM_FPS);
    }
}
