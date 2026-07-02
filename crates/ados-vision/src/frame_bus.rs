//! The `vision-frames.sock` broadcast.
//!
//! The engine fans every published [`FrameDescriptor`] out on an in-process
//! broadcast channel ([`crate::engine::VisionEngine::subscribe_frames`]). Plugins
//! reach those through the `vision.sock` request/response bridge, but an on-box
//! Rust service (the world-model capture service) is not a plugin and should not
//! speak the plugin RPC. This module bridges the in-process channel onto a
//! Unix-socket broadcast so any on-box consumer can subscribe to the descriptor
//! stream, then map the same `/dev/shm` ring the descriptor names and read the
//! slot itself.
//!
//! Wire shape: each descriptor is one length-prefixed msgpack frame — the same
//! 4-byte big-endian length prefix the state and detection sockets use
//! ([`ados_protocol::frame`]), with a msgpack-named-map body
//! ([`FrameDescriptor::to_msgpack`]). Only the small descriptor crosses the
//! socket; the pixels stay in shared memory. The socket is a last-state
//! broadcast ([`IpcBroadcast`] with `keep_last = true`), so a late subscriber
//! immediately receives the most recent descriptor instead of waiting for the
//! next frame (a descriptor whose slot has since been recycled simply fails the
//! seqlock check on read and is skipped).
//!
//! It is purely additive: the `vision.sock` plugin bridge and the engine's own
//! frame-publish path are untouched. A subscriber whose queue fills is dropped
//! by the broadcast server (slow-client policy), so the engine never backs up
//! behind a stalled reader.

use std::sync::Arc;

use ados_protocol::frame::{encode_frame, FrameError};
use ados_protocol::framebus::FrameDescriptor;
use ados_protocol::ipc::IpcBroadcast;
use ados_protocol::state::STATE_V2_MAX_FRAME;

use crate::engine::VisionEngine;

/// Per-client outbound queue depth for the frames broadcast. Descriptors are
/// tiny and arrive at camera rate (tens per second); 64 frames is roughly a
/// couple of seconds of headroom before a stalled subscriber is pruned.
const FRAMES_QUEUE_DEPTH: usize = 64;

/// Encode a [`FrameDescriptor`] as a complete broadcast frame: a 4-byte
/// big-endian length prefix followed by the msgpack-named-map body. Bounded by
/// the state-frame cap (a descriptor is far smaller than a telemetry snapshot,
/// so the same generous ceiling applies).
pub fn encode_descriptor_frame(desc: &FrameDescriptor) -> anyhow::Result<Vec<u8>> {
    let body = desc
        .to_msgpack()
        .map_err(|e| anyhow::anyhow!("encode frame descriptor: {e}"))?;
    encode_frame(&body, STATE_V2_MAX_FRAME)
        .map_err(|e: FrameError| anyhow::anyhow!("frame descriptor ({} bytes): {e}", body.len()))
}

/// Bind `vision-frames.sock` and forward every published [`FrameDescriptor`] to
/// it as a length-prefixed msgpack frame, until `cancel` is notified. Mirrors
/// [`crate::detection_bus::serve`]; callers run it inside their own
/// `tokio::spawn`.
pub async fn serve(
    engine: Arc<VisionEngine>,
    socket_path: &str,
    cancel: Arc<tokio::sync::Notify>,
) -> anyhow::Result<()> {
    // keep_last = true so a consumer that connects mid-stream gets the latest
    // descriptor immediately. inbound = None: broadcast only.
    let (server, _no_inbound) =
        IpcBroadcast::bind(socket_path, FRAMES_QUEUE_DEPTH, true, None).await?;
    tracing::info!(path = %socket_path, "vision_frames_sock_listening");

    let mut rx = engine.subscribe_frames();
    loop {
        tokio::select! {
            recv = rx.recv() => {
                match recv {
                    Ok(desc) => match encode_descriptor_frame(&desc) {
                        Ok(frame) => server.broadcast(frame).await,
                        Err(e) => tracing::warn!(error = %e, "vision_frames_encode_failed"),
                    },
                    // A lagged subscriber skips to the tail (latest-wins, like the
                    // rings). The channel closing means the engine is gone.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            _ = cancel.notified() => break,
        }
    }
    // Dropping `server` unbinds the socket and aborts client tasks.
    tracing::info!(path = %socket_path, "vision_frames_sock_stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::framebus::{FrameFormat, FRAMEBUS_DESCRIPTOR_VERSION};
    use ados_protocol::ipc::{connect_with_retry, read_length_prefixed};
    use std::time::Duration;
    use tokio::sync::Notify;

    fn sample_descriptor() -> FrameDescriptor {
        FrameDescriptor {
            v: FRAMEBUS_DESCRIPTOR_VERSION,
            camera_id: "uvc-0".into(),
            frame_id: 7,
            ts_ms: 1_700_000_000_000,
            width: 640,
            height: 480,
            format: FrameFormat::Rgb24,
            shm_name: "ados-vision-uvc-0".into(),
            slot: 2,
            seq: 42,
            byte_len: (640 * 480 * 3) as u32,
        }
    }

    #[test]
    fn descriptor_frame_round_trips_through_length_prefix() {
        let desc = sample_descriptor();
        let frame = encode_descriptor_frame(&desc).unwrap();
        let len = u32::from_be_bytes(frame[..4].try_into().unwrap()) as usize;
        assert_eq!(len, frame.len() - 4);
        let decoded = FrameDescriptor::from_msgpack(&frame[4..]).unwrap();
        assert_eq!(decoded, desc);
    }

    #[tokio::test]
    async fn published_frame_reaches_a_socket_subscriber() {
        let engine = crate::engine::VisionEngine::new(Box::new(crate::backend::MockBackend), 4);
        let cancel = Arc::new(Notify::new());

        let dir = tempfile::tempdir().unwrap();
        let sock = dir
            .path()
            .join("vision-frames.sock")
            .to_string_lossy()
            .to_string();

        let server_engine = engine.clone();
        let server_cancel = cancel.clone();
        let server_sock = sock.clone();
        let server = tokio::spawn(async move {
            serve(server_engine, &server_sock, server_cancel)
                .await
                .unwrap();
        });

        let mut client = connect_with_retry(&sock, 50, Duration::from_millis(20))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Publishing a frame into the engine ring emits a descriptor on the bus.
        let pixels = vec![0u8; FrameFormat::Rgb24.frame_bytes(8, 8)];
        let desc = engine
            .publish_frame("uvc-0", 1, 1_000, 8, 8, FrameFormat::Rgb24, &pixels)
            .await
            .unwrap();

        let payload = read_length_prefixed(&mut client, STATE_V2_MAX_FRAME, true)
            .await
            .unwrap()
            .expect("a frame");
        let got = FrameDescriptor::from_msgpack(&payload).unwrap();
        assert_eq!(got.camera_id, "uvc-0");
        assert_eq!(got.shm_name, desc.shm_name);
        assert_eq!(got.seq, desc.seq);

        cancel.notify_waiters();
        let _ = server.await;
    }
}
