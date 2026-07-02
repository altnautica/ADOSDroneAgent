//! Regression net for the video-pipeline → vision-engine tap seam.
//!
//! The failure this guards against: two services in one build silently disagree
//! on the tap wire format, so decoded frames never cross the socket and the
//! vision engine (and everything downstream of it) sees nothing — with no error
//! anywhere. This test wires the REAL producer half (`ados_video::tap`) to the
//! REAL consumer half (`ados_vision::source::TapSource`) over a Unix socket and
//! asserts that every frame's geometry, format, and exact pixel bytes survive
//! the round trip. If any hop breaks, the assertions fail loudly.
//!
//! The path exercised end-to-end:
//!
//! ```text
//! duplex (fake ffmpeg stdout)
//!   -> run_vision_tap_server  (reads fixed-size rawvideo, prepends the ADVT header, serves the socket)
//!   -> TapSource              (connects as the engine client, decodes the ADVT header, reads the pixels)
//! ```
//!
//! Fully cross-platform: the tap seam is a plain Unix socket carrying
//! `[header][pixels]`, so this touches no `/dev/shm` ring and compiles + runs on
//! macOS and Linux alike.

use std::time::Duration;

use ados_protocol::framebus::FrameFormat;
use ados_video::tap::{bind_vision_tap, run_vision_tap_server};
use ados_vision::source::{FrameSource, RawFrame, TapSource};
use tokio::io::{AsyncWriteExt, DuplexStream};
use tokio::sync::mpsc;
use tokio::time::timeout;

/// Assert one frame crossed the tap with its geometry, format, and every pixel
/// byte intact. Each test frame is a uniform fill, so an exact-vector compare is
/// a literal "the pixel bytes are unchanged" check.
fn assert_frame_intact(got: &RawFrame, width: u32, height: u32, fmt: FrameFormat, fill: u8) {
    let expected = vec![fill; (width as usize) * (height as usize) * 3];
    assert_eq!(
        got.width, width,
        "frame(fill={fill}): width did not survive the tap"
    );
    assert_eq!(
        got.height, height,
        "frame(fill={fill}): height did not survive the tap"
    );
    assert_eq!(
        got.format, fmt,
        "frame(fill={fill}): pixel format did not survive the tap"
    );
    assert_eq!(
        got.data.len(),
        expected.len(),
        "frame(fill={fill}): byte length did not survive the tap"
    );
    assert_eq!(
        got.data, expected,
        "frame(fill={fill}): pixel bytes were altered crossing the tap"
    );
}

/// The tap seam carries each frame across intact: several distinct rawvideo
/// frames fed into the reframer come back out of the real `TapSource` with the
/// exact width/height/format decoded from the ADVT header and the exact pixels.
#[tokio::test]
async fn tap_advt_roundtrip_delivers_each_frame_intact() {
    const W: u32 = 8;
    const H: u32 = 8;
    const FORMAT: FrameFormat = FrameFormat::Rgb24;
    // Number of distinct frames to push (>= 3). Frame k is filled with byte k.
    const FRAMES: u8 = 5;

    let frame_size = (W as usize) * (H as usize) * 3; // rgb24: 3 bytes/px

    let dir = tempfile::tempdir().expect("temp dir");
    let sink = dir.path().join("vision-tap.sock");
    let sink_str = sink.to_string_lossy().to_string();

    // The fake ffmpeg stdout: an in-memory pipe. We write raw frames into `feed`;
    // the reframer reads them off `ffmpeg_stdout` and serves ADVT frames.
    let (mut feed, ffmpeg_stdout): (DuplexStream, DuplexStream) = tokio::io::duplex(4096);

    // The producer half under test: bind, then serve the reframer.
    let listener = bind_vision_tap(&sink_str).expect("bind vision tap socket");
    let server = tokio::spawn(run_vision_tap_server(listener, ffmpeg_stdout, FORMAT, W, H));

    // The consumer half under test, driven in its own task so a read is never
    // cancelled mid-frame (TapSource's read_exact is not cancel-safe). It forwards
    // each decoded frame over an mpsc channel, whose `recv` IS cancel-safe, so the
    // test body can time out cleanly without corrupting the socket read state.
    let (frame_tx, mut frame_rx) = mpsc::channel::<RawFrame>(64);
    let reader_sink = sink_str.clone();
    let reader = tokio::spawn(async move {
        let mut src = TapSource::new("cam-test", reader_sink);
        // Sanity: the source reports the camera id it was constructed with.
        assert_eq!(src.camera_id(), "cam-test");
        // Loop while frames decode; an Err (socket closed at teardown) ends it.
        while let Ok(frame) = src.next_frame().await {
            if frame_tx.send(frame).await.is_err() {
                break; // test finished, receiver dropped
            }
        }
    });

    // Frame 0: establish the connection. The engine (TapSource) connects lazily on
    // its first read, so a frame written before it has connected is dropped by the
    // reframer (newest-frame-wins, no engine attached yet). Re-send frame 0 until
    // one is delivered. Every write here is the same value, so a coalesced or
    // duplicated delivery is still, unambiguously, frame 0.
    let zero = vec![0u8; frame_size];
    let mut first: Option<RawFrame> = None;
    for _ in 0..40 {
        feed.write_all(&zero).await.expect("feed frame 0");
        feed.flush().await.expect("flush frame 0");
        match timeout(Duration::from_millis(250), frame_rx.recv()).await {
            Ok(Some(frame)) => {
                first = Some(frame);
                break;
            }
            Ok(None) => panic!("tap reader task ended before delivering the first frame"),
            Err(_) => continue, // engine not connected yet; re-send
        }
    }
    let first = first.expect("the tap never delivered a frame (the engine seam is broken)");
    assert_frame_intact(&first, W, H, FORMAT, 0);

    // Drain any surplus frame-0 copies that the connection handshake left buffered,
    // so the strict lockstep below sees exactly one frame per write.
    while let Ok(Some(_)) = timeout(Duration::from_millis(300), frame_rx.recv()).await {}

    // Frames 1..FRAMES: strict lockstep, one frame in flight at a time. Because we
    // do not write the next frame until the current one has been received, the
    // reframer's newest-frame-wins watch can never coalesce two distinct frames
    // together — each write is delivered whole, and we assert its exact bytes.
    for k in 1..FRAMES {
        let frame_k = vec![k; frame_size];
        feed.write_all(&frame_k).await.expect("feed frame");
        feed.flush().await.expect("flush frame");

        let got = timeout(Duration::from_secs(5), frame_rx.recv())
            .await
            .expect("timed out waiting for a tapped frame; a hop in the tap seam is broken")
            .expect("tap reader task ended mid-stream");
        assert_frame_intact(&got, W, H, FORMAT, k);
    }

    // Teardown: closing the fake ffmpeg stdout drives the reframer to EOF; then
    // stop both tasks and let the temp dir clean up the socket.
    drop(feed);
    server.abort();
    reader.abort();
    let _ = server.await;
    let _ = reader.await;
}
