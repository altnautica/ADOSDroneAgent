//! The per-connection push streams of `vision.sock`: frame descriptors
//! (`vision.deliver`) and detection batches (`vision.deliver_detection`).

use std::sync::Arc;

use ados_protocol::framebus::{methods, DetectionBatch, FrameDescriptor};
use ados_protocol::plugin::{Envelope, PROTOCOL_VERSION};
use anyhow::{anyhow, Result};
use rmpv::Value;
use tokio::io::AsyncWriteExt;

use crate::engine::VisionEngine;

/// Spawn the per-connection frame-descriptor push task. Every published
/// descriptor (optionally filtered to one camera) is wrapped in a
/// `vision.deliver` event envelope and written to the connection. A lagged
/// subscriber skips to the tail (latest-wins); a write error ends the task.
pub(super) fn spawn_frame_push(
    engine: Arc<VisionEngine>,
    writer: Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
    camera_filter: Option<String>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut rx = engine.subscribe_frames();
        loop {
            match rx.recv().await {
                Ok(desc) => {
                    if let Some(want) = &camera_filter {
                        if &desc.camera_id != want {
                            continue;
                        }
                    }
                    let frame = match deliver_frame(&desc) {
                        Ok(f) => f,
                        Err(_) => continue,
                    };
                    let mut w = writer.lock().await;
                    if w.write_all(&frame).await.is_err() || w.flush().await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

/// Build a `vision.deliver` event frame carrying the descriptor as a binary
/// `descriptor` field, ready to write.
pub(super) fn deliver_frame(desc: &FrameDescriptor) -> Result<Vec<u8>> {
    let bytes = desc
        .to_msgpack()
        .map_err(|e| anyhow!("encode descriptor: {e}"))?;
    let env = Envelope {
        version: PROTOCOL_VERSION,
        kind: "event".to_string(),
        method: methods::DELIVER_FRAME.to_string(),
        capability: "vision.frame.read".to_string(),
        args: Value::Map(vec![(Value::from("descriptor"), Value::Binary(bytes))]),
        request_id: format!("vis-frame-{}", desc.seq),
        token: String::new(),
        error: None,
    };
    env.encode_frame()
        .map_err(|e| anyhow!("encode deliver envelope: {e}"))
}

/// Spawn the per-connection detection-batch push task. Every published batch
/// (optionally filtered to one camera) is wrapped in a `vision.deliver_detection`
/// event envelope and written to the connection. A lagged subscriber skips to
/// the tail (latest-wins); a write error ends the task.
pub(super) fn spawn_detection_push(
    engine: Arc<VisionEngine>,
    writer: Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
    camera_filter: Option<String>,
    with_embeddings: bool,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut rx = engine.subscribe_detections();
        loop {
            match rx.recv().await {
                Ok(mut batch) => {
                    if let Some(want) = &camera_filter {
                        if &batch.camera_id != want {
                            continue;
                        }
                    }
                    // Embeddings ride only to a connection that asked for them.
                    if !with_embeddings {
                        crate::engine::strip_embeddings(&mut batch);
                    }
                    let frame = match deliver_detection(&batch) {
                        Ok(f) => f,
                        Err(_) => continue,
                    };
                    let mut w = writer.lock().await;
                    if w.write_all(&frame).await.is_err() || w.flush().await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

/// Build a `vision.deliver_detection` event frame carrying the encoded
/// `DetectionBatch` as a binary `batch` field, ready to write.
pub(super) fn deliver_detection(batch: &DetectionBatch) -> Result<Vec<u8>> {
    let bytes = batch
        .to_msgpack()
        .map_err(|e| anyhow!("encode detection batch: {e}"))?;
    let env = Envelope {
        version: PROTOCOL_VERSION,
        kind: "event".to_string(),
        method: methods::DELIVER_DETECTION.to_string(),
        capability: "vision.detection.subscribe".to_string(),
        args: Value::Map(vec![(Value::from("batch"), Value::Binary(bytes))]),
        request_id: format!("vis-det-{}-{}", batch.camera_id, batch.frame_id),
        token: String::new(),
        error: None,
    };
    env.encode_frame()
        .map_err(|e| anyhow!("encode deliver detection envelope: {e}"))
}
