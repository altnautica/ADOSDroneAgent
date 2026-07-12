//! The `vision-detections.sock` broadcast.
//!
//! The engine already fans every published [`DetectionBatch`] out on an
//! in-process broadcast channel ([`crate::engine::VisionEngine::subscribe_detections`]).
//! Plugins reach those through the `vision.sock` request/response bridge, but a
//! browser cannot speak that socket. This module bridges the in-process channel
//! onto a Unix-socket broadcast so the agent's API process can subscribe and
//! forward batches to the GCS over a WebSocket.
//!
//! Wire shape: each batch is one length-prefixed msgpack frame — the same
//! 4-byte big-endian length prefix the state and MAVLink sockets use
//! ([`ados_protocol::frame`]), with a msgpack-named-map body
//! ([`DetectionBatch::to_msgpack`]). The socket is a last-state broadcast
//! ([`IpcBroadcast`] with `keep_last = true`), so a late subscriber immediately
//! receives the most recent batch instead of waiting for the next detection.
//!
//! It is purely additive: the existing `vision.sock` plugin bridge and the
//! engine's own detection-publish path are untouched. A subscriber whose queue
//! fills is dropped by the broadcast server (slow-client policy), so the engine
//! never backs up behind a stalled reader.

use std::sync::Arc;

use ados_protocol::frame::{encode_frame, FrameError};
use ados_protocol::framebus::DetectionBatch;
use ados_protocol::ipc::IpcBroadcast;
use ados_protocol::state::STATE_V2_MAX_FRAME;

use crate::engine::VisionEngine;

/// Per-client outbound queue depth for the detections broadcast. Detection
/// batches are small and arrive at inference rate (a few to tens per second);
/// 64 frames is roughly a couple of seconds of headroom before a stalled
/// browser subscriber is pruned.
const DETECTIONS_QUEUE_DEPTH: usize = 64;

/// Encode a [`DetectionBatch`] as a complete broadcast frame: a 4-byte
/// big-endian length prefix followed by the msgpack-named-map body. Bounded by
/// the state-frame cap (detection batches are far smaller than telemetry, so
/// the same generous ceiling applies).
pub fn encode_batch_frame(batch: &DetectionBatch) -> anyhow::Result<Vec<u8>> {
    let body = batch
        .to_msgpack()
        .map_err(|e| anyhow::anyhow!("encode detection batch: {e}"))?;
    encode_frame(&body, STATE_V2_MAX_FRAME).map_err(|e: FrameError| {
        anyhow::anyhow!("frame detection batch ({} bytes): {e}", body.len())
    })
}

/// Bind `vision-detections.sock` and forward every published [`DetectionBatch`]
/// to it as a length-prefixed msgpack frame, until `cancel` is notified.
///
/// Returns once the socket is bound and the forward loop has started — it does
/// not block; the forwarding runs in a spawned task whose handle is awaited via
/// `cancel`. (Callers run this inside a `tokio::spawn` of their own, mirroring
/// `visionsock::serve`.)
pub async fn serve(
    engine: Arc<VisionEngine>,
    socket_path: &str,
    cancel: Arc<tokio::sync::Notify>,
) -> anyhow::Result<()> {
    // keep_last = true so a browser that connects after a detection still
    // gets the latest box set immediately. inbound = None: broadcast only.
    let (server, _no_inbound) =
        IpcBroadcast::bind(socket_path, DETECTIONS_QUEUE_DEPTH, true, None).await?;
    tracing::info!(path = %socket_path, "vision_detections_sock_listening");

    let mut rx = engine.subscribe_detections();
    loop {
        tokio::select! {
            recv = rx.recv() => {
                match recv {
                    Ok(batch) => {
                        match encode_batch_frame(&batch) {
                            Ok(frame) => server.broadcast(frame).await,
                            Err(e) => {
                                tracing::warn!(error = %e, "vision_detections_encode_failed");
                            }
                        }
                    }
                    // A lagged subscriber skips to the tail (latest-wins, like
                    // the rings). The channel closing means the engine is gone.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            _ = cancel.notified() => break,
        }
    }
    // Dropping `server` unbinds the socket and aborts client tasks.
    tracing::info!(path = %socket_path, "vision_detections_sock_stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::framebus::{BoundingBox, Detection, VISION_DETECTION_VERSION};
    use ados_protocol::ipc::{connect_with_retry, read_length_prefixed};
    use std::time::Duration;
    use tokio::sync::Notify;

    fn sample_batch() -> DetectionBatch {
        DetectionBatch {
            v: VISION_DETECTION_VERSION,
            model_id: "com.example.weeds".into(),
            camera_id: "uvc-0".into(),
            frame_id: 7,
            ts_ms: 1_700_000_000_000,
            frame_width: 640,
            frame_height: 480,
            detections: vec![Detection {
                bbox: BoundingBox {
                    x: 12.0,
                    y: 20.0,
                    width: 64.0,
                    height: 32.0,
                },
                class_label: "weed".into(),
                confidence: 0.87,
                track_id: Some(3),
                assoc_confidence: None,
                lock_state: None,
                attributes: None,
            }],
        }
    }

    #[test]
    fn batch_frame_round_trips_through_length_prefix() {
        let batch = sample_batch();
        let frame = encode_batch_frame(&batch).unwrap();
        // 4-byte big-endian length prefix.
        let len = u32::from_be_bytes(frame[..4].try_into().unwrap()) as usize;
        assert_eq!(len, frame.len() - 4);
        let decoded = DetectionBatch::from_msgpack(&frame[4..]).unwrap();
        assert_eq!(decoded, batch);
    }

    #[tokio::test]
    async fn published_detection_reaches_a_socket_subscriber() {
        let engine = crate::engine::VisionEngine::new(Box::new(crate::backend::MockBackend), 4);
        let cancel = Arc::new(Notify::new());

        let dir = tempfile::tempdir().unwrap();
        let sock = dir
            .path()
            .join("vision-detections.sock")
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

        // Connect a subscriber, then publish.
        let mut client = connect_with_retry(&sock, 50, Duration::from_millis(20))
            .await
            .unwrap();
        // Small wait so the accept loop registers the client before publish.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let batch = sample_batch();
        engine.publish_detection(batch.clone());

        let payload = read_length_prefixed(&mut client, STATE_V2_MAX_FRAME, true)
            .await
            .unwrap()
            .expect("a frame");
        let got = DetectionBatch::from_msgpack(&payload).unwrap();
        assert_eq!(got, batch);

        cancel.notify_waiters();
        let _ = server.await;
    }

    #[tokio::test]
    async fn late_subscriber_gets_the_last_batch_replayed() {
        let engine = crate::engine::VisionEngine::new(Box::new(crate::backend::MockBackend), 4);
        let cancel = Arc::new(Notify::new());

        let dir = tempfile::tempdir().unwrap();
        let sock = dir
            .path()
            .join("vision-detections.sock")
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

        // Wait for the socket to bind (a probe connect that we immediately
        // drop), then publish BEFORE any lasting client connects.
        connect_with_retry(&sock, 50, Duration::from_millis(20))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;

        let batch = sample_batch();
        engine.publish_detection(batch.clone());
        // Let the forward loop store the batch as last-state.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // A new subscriber that connects AFTER the publish still gets it.
        let mut late = connect_with_retry(&sock, 50, Duration::from_millis(20))
            .await
            .unwrap();
        let payload = read_length_prefixed(&mut late, STATE_V2_MAX_FRAME, true)
            .await
            .unwrap()
            .expect("replayed last batch");
        let got = DetectionBatch::from_msgpack(&payload).unwrap();
        assert_eq!(got, batch);

        cancel.notify_waiters();
        let _ = server.await;
    }
}
