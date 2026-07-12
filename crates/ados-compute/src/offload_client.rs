//! The drone-side subscriber to a node's detection return stream.
//!
//! An NPU-less drone opens an offload session on a paired compute node, then
//! subscribes to the node's per-session detection WebSocket
//! ([`crate::offload_ws`]). [`stream_offload_detections`] connects, decodes each
//! [`OffloadDetectionBatch`], and hands it to a channel the return bridge drains.
//! Local-first: the node is reached at its LAN address.

use ados_protocol::offload::OffloadDetectionBatch;
use anyhow::{anyhow, Result};
use futures_util::StreamExt;
use std::sync::Arc;
use tokio_tungstenite::tungstenite::Message;

/// Connect to a node's detection return stream at `ws_url` (e.g.
/// `ws://compute.local:8092/ws/offload/<session>`) and forward each decoded batch
/// onto `sink`. Returns when the stream closes, `cancel` is notified, the sink is
/// dropped, or the connection fails. A batch that fails to decode (a version this
/// build does not speak) is logged and skipped, not fatal.
pub async fn stream_offload_detections(
    ws_url: &str,
    sink: tokio::sync::mpsc::Sender<OffloadDetectionBatch>,
    cancel: Arc<tokio::sync::Notify>,
) -> Result<()> {
    let (mut ws, _resp) = tokio_tungstenite::connect_async(ws_url)
        .await
        .map_err(|e| anyhow!("connect {ws_url}: {e}"))?;
    tracing::info!(url = ws_url, "offload detection stream connected");

    loop {
        tokio::select! {
            msg = ws.next() => match msg {
                Some(Ok(Message::Binary(bytes))) => {
                    match OffloadDetectionBatch::from_msgpack(&bytes) {
                        Ok(batch) => {
                            if sink.send(batch).await.is_err() {
                                // The bridge stopped draining; nothing to feed.
                                break;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "offload detection batch decode failed; skipped");
                        }
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Err(e)) => {
                    tracing::info!(error = %e, "offload detection stream ended");
                    break;
                }
                Some(Ok(_)) => {} // ping/pong/text — ignore
            },
            _ = cancel.notified() => break,
        }
    }
    tracing::info!(url = ws_url, "offload detection stream closed");
    Ok(())
}
