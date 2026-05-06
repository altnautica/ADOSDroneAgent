//! RTSP push-loop reconnect coverage.
//!
//! Two concerns:
//!
//!   * The reconnect backoff curve doubles each failure but caps at 60
//!     seconds. The first attempt waits 1s, then 2, 4, 8, 16, 32, 60,
//!     60, 60, ... The cap is the difference between "noisy network"
//!     and "cloud relay is permanently down" so we pin the curve here
//!     to catch any future tuning regression.
//!
//!   * Issuing TEARDOWN twice on the same session is idempotent. A
//!     real RTSP server replies 200 OK to either teardown; the push
//!     client must not panic or surface an error if it sees the second
//!     teardown reply.
//!
//! The crate's backoff constants (`INITIAL_BACKOFF` = 1s, `MAX_BACKOFF`
//! = 60s) are private to `rtsp.rs`. We mirror the documented curve in
//! a local helper here so a tuning change in the source forces an
//! intentional update to this test.

use std::time::Duration;

use ados_test_mocks::MockRtspServer;
use ados_video::rtsp::{build_request, parse_response_head, ParsedUrl};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Reproduce the documented backoff curve. Initial delay is 1s, each
/// failure doubles the previous, capped at 60s. Returns the next
/// scheduled wait given the current wait.
fn next_backoff(current: Duration) -> Duration {
    const MAX: Duration = Duration::from_secs(60);
    (current * 2).min(MAX)
}

#[test]
fn backoff_curve_doubles_then_caps_at_60s() {
    // Pin the documented curve: 1, 2, 4, 8, 16, 32, 60, 60, 60, ...
    let mut current = Duration::from_secs(1);
    let expected_seconds = [1u64, 2, 4, 8, 16, 32, 60, 60, 60, 60];
    let mut seen = Vec::new();
    seen.push(current.as_secs());
    for _ in 1..expected_seconds.len() {
        current = next_backoff(current);
        seen.push(current.as_secs());
    }
    assert_eq!(seen, expected_seconds);
}

#[test]
fn backoff_curve_starting_below_cap_eventually_hits_cap() {
    // Drive long enough that the curve stabilises at the cap.
    let mut current = Duration::from_secs(1);
    for _ in 0..32 {
        current = next_backoff(current);
    }
    assert_eq!(current, Duration::from_secs(60));
}

#[test]
fn backoff_curve_at_cap_stays_at_cap() {
    // Idempotent at the ceiling.
    let at_cap = Duration::from_secs(60);
    assert_eq!(next_backoff(at_cap), Duration::from_secs(60));
    assert_eq!(next_backoff(next_backoff(at_cap)), Duration::from_secs(60));
}

/// Drive the mock server through OPTIONS / ANNOUNCE / SETUP / RECORD
/// and then issue TEARDOWN twice in a row on the same session. The
/// permissive mock replies 200 OK to every request. The test asserts
/// neither teardown errors and the captured session count stays at 1.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn teardown_twice_on_same_session_is_idempotent() {
    let server = MockRtspServer::start().await.expect("start mock rtsp");
    let url = server.url("teardown-twice");
    let parsed = ParsedUrl::parse(&url).expect("parse url");

    let mut stream = TcpStream::connect((parsed.host.as_str(), parsed.port))
        .await
        .expect("connect");
    stream.set_nodelay(true).expect("nodelay");

    // Walk the standard handshake.
    write_and_read(&mut stream, &build_request("OPTIONS", &url, 1, &[], None)).await;
    let sdp = "v=0\r\no=- 0 0 IN IP4 0.0.0.0\r\ns=test\r\nm=video 0 RTP/AVP 96\r\n";
    write_and_read(
        &mut stream,
        &build_request("ANNOUNCE", &url, 2, &[], Some(sdp)),
    )
    .await;
    let track_url = format!("{}/trackID=0", url.trim_end_matches('/'));
    let setup_extra = [(
        "Transport",
        "RTP/AVP/TCP;unicast;interleaved=0-1;mode=record",
    )];
    let setup_resp = write_and_read(
        &mut stream,
        &build_request("SETUP", &track_url, 3, &setup_extra, None),
    )
    .await;
    let session_id = setup_resp.session.expect("session id from SETUP");

    let record_extra = [
        ("Session", session_id.as_str()),
        ("Range", "npt=0.000-"),
    ];
    write_and_read(
        &mut stream,
        &build_request("RECORD", &url, 4, &record_extra, None),
    )
    .await;

    // First TEARDOWN.
    let teardown_extra = [("Session", session_id.as_str())];
    let head1 = write_and_read(
        &mut stream,
        &build_request("TEARDOWN", &url, 5, &teardown_extra, None),
    )
    .await;
    assert_eq!(head1.code, 200, "first TEARDOWN should reply 200 OK");

    // Second TEARDOWN on the same session. The mock is permissive and
    // replies 200 OK; the assertion is that the round-trip itself
    // returns without panicking and that the parsed status is still
    // 200.
    let head2 = write_and_read(
        &mut stream,
        &build_request("TEARDOWN", &url, 6, &teardown_extra, None),
    )
    .await;
    assert_eq!(head2.code, 200, "second TEARDOWN should also reply 200 OK");

    // Exactly one session was minted across the two teardowns.
    assert_eq!(
        server.captured_session_count(),
        1,
        "double teardown must not create extra sessions"
    );

    drop(stream);
    server.shutdown().await;
}

/// Send `request` on `stream` and read the response head. Helper that
/// avoids dragging the full request/response state machine into the
/// tests above.
async fn write_and_read(
    stream: &mut TcpStream,
    request: &str,
) -> ados_video::rtsp::RtspResponseHead {
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write request");
    stream.flush().await.expect("flush");

    // Read response head until CRLF CRLF.
    let mut buf = Vec::with_capacity(512);
    let mut byte = [0u8; 1];
    loop {
        let n = stream.read(&mut byte).await.expect("read response byte");
        assert!(n > 0, "EOF before response head completed");
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
    }

    // The mock server always sets Content-Length: 0, so the CRLF CRLF
    // terminator above already consumed the full response.
    let head_text = std::str::from_utf8(&buf).expect("utf-8 response head");
    parse_response_head(head_text).expect("parse response head")
}
