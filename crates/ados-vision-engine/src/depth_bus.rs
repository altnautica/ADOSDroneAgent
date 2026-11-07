//! The `vision-depth.sock` broadcast: every depth grid and landing-zone frame
//! the engine computes, re-published as one length-prefixed msgpack record
//! ([`ados_protocol::vision_depth::DepthRecord`]) so the control plane can
//! serve them as the `depth` and `surfaces` nav kinds.
//!
//! Broadcast only, not last-state: a subscriber that connects late waits for
//! the next frame (at most one depth interval) rather than receiving a stale
//! grid it would have to age out. A subscriber whose queue fills is dropped
//! by the broadcast server, so the engine never backs up behind it.

use std::sync::Arc;

use ados_protocol::frame::encode_frame;
use ados_protocol::ipc::IpcBroadcast;
use ados_protocol::vision_depth::DepthRecord;

use crate::engine::VisionEngine;

/// Per-client outbound queue depth: a few seconds of depth and surfaces
/// frames at their highest rate.
const DEPTH_QUEUE_DEPTH: usize = 32;

/// The largest record: a grid is under a kilobyte, a zone list a few.
const MAX_RECORD: usize = 64 * 1024;

/// Encode a record as a complete broadcast frame (length prefix + body).
pub fn encode_record_frame(record: &DepthRecord) -> anyhow::Result<Vec<u8>> {
    let body = record.to_msgpack();
    encode_frame(&body, MAX_RECORD)
        .map_err(|e| anyhow::anyhow!("frame depth record ({} bytes): {e}", body.len()))
}

/// Bind `vision-depth.sock` and forward every depth record to it until
/// `cancel`.
pub async fn serve(
    engine: Arc<VisionEngine>,
    socket_path: &str,
    cancel: ados_protocol::shutdown::Shutdown,
) -> anyhow::Result<()> {
    let (server, _no_inbound) =
        IpcBroadcast::bind(socket_path, DEPTH_QUEUE_DEPTH, false, None).await?;
    tracing::info!(path = %socket_path, "vision_depth_sock_listening");
    let mut rx = engine.subscribe_depth();
    loop {
        tokio::select! {
            recv = rx.recv() => match recv {
                Ok(record) => match encode_record_frame(&record) {
                    Ok(frame) => server.broadcast(frame.into()).await,
                    Err(e) => tracing::warn!(error = %e, "vision_depth_encode_failed"),
                },
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            _ = cancel.wait() => break,
        }
    }
    tracing::info!(path = %socket_path, "vision_depth_sock_stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::ipc::{connect_with_retry, read_length_prefixed};
    use ados_protocol::vision_depth::{SurfaceZone, SurfacesFrame};
    use std::time::Duration;

    #[tokio::test]
    async fn a_published_record_reaches_a_subscriber() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("vision-depth.sock");
        let sock_str = sock.to_str().unwrap().to_string();
        let engine = VisionEngine::new(Box::new(crate::backend::MockBackend), 4);
        let cancel = ados_protocol::shutdown::Shutdown::new();
        let server = {
            let engine = engine.clone();
            let cancel = cancel.clone();
            let sock_str = sock_str.clone();
            tokio::spawn(async move { serve(engine, &sock_str, cancel).await })
        };
        let mut client = connect_with_retry(&sock_str, 50, Duration::from_millis(20))
            .await
            .unwrap();
        // Let the accept loop register the client before publishing.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let record = DepthRecord::Surfaces(SurfacesFrame {
            camera_id: "main".into(),
            mono_ns: 3,
            zones: vec![SurfaceZone {
                pts: vec![[0.1, 0.9], [0.4, 0.9], [0.4, 1.0]],
                score: 0.25,
                slope_deg: 2.0,
            }],
        });
        engine.publish_depth(record.clone());
        let body = tokio::time::timeout(
            Duration::from_secs(5),
            read_length_prefixed(&mut client, MAX_RECORD, true),
        )
        .await
        .unwrap()
        .unwrap()
        .expect("a record");
        assert_eq!(DepthRecord::from_msgpack(&body).unwrap(), record);
        cancel.trigger();
        server.await.unwrap().unwrap();
    }
}
