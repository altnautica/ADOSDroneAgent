//! Where the vision engine's live backend runs its detector.
//!
//! The engine folds its `backend` name and `backend_inference_capable` into
//! every `vision.list_models` reply, whether or not a model is registered, so
//! one request on its socket answers whether a real detector runs and on what
//! silicon. Every surface that reports the perception tier (the control
//! front's status routes and the cloud heartbeat) reads it through
//! [`query_live_detector`], so they derive the tier from the same live answer.

use std::path::Path;
use std::time::Duration;

use rmpv::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::frame::{decode_len, HEADER_SIZE, PLUGIN_MAX_FRAME};
use crate::framebus::methods;
use crate::plugin::{Envelope, PROTOCOL_VERSION};

/// The engine's backend name for its in-process ONNX runtime: the detector
/// runs on the companion's CPU.
pub const CPU_BACKEND: &str = "onnx";

/// Wall bound on one query, covering connect, write and both reads. A healthy
/// engine answers off the local socket in well under a millisecond; this only
/// stops an engine that accepts and never replies from stalling the caller.
pub const QUERY_TIMEOUT: Duration = Duration::from_secs(2);

/// Where the vision engine's live backend runs the detector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveDetector {
    /// No detector runs: the engine is unreachable (vision disabled, not up
    /// yet, or mid-restart), or its backend runs no real inference (the mock,
    /// or an unavailable backend).
    Off,
    /// A real backend on an accelerator (an NPU or accelerator sidecar).
    Accelerator,
    /// The in-process ONNX backend: the detector runs on the CPU.
    Cpu,
}

/// The detector a `vision.list_models` reply describes.
pub fn detector_from_reply(reply: &Value) -> LiveDetector {
    let field = |key: &str| {
        reply
            .as_map()
            .and_then(|m| m.iter().find(|(k, _)| k.as_str() == Some(key)))
            .map(|(_, v)| v)
    };
    let capable = field("backend_inference_capable")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !capable {
        return LiveDetector::Off;
    }
    match field("backend").and_then(Value::as_str) {
        Some(CPU_BACKEND) => LiveDetector::Cpu,
        _ => LiveDetector::Accelerator,
    }
}

/// Ask the engine on `socket_path` where its detector runs. An unreachable,
/// silent or failing engine reads [`LiveDetector::Off`]: with no answer there
/// is no live inference to report, and a remembered one could be stale.
pub async fn query_live_detector(socket_path: &Path) -> LiveDetector {
    match tokio::time::timeout(QUERY_TIMEOUT, list_models(socket_path)).await {
        Ok(Some(reply)) => detector_from_reply(&reply),
        _ => LiveDetector::Off,
    }
}

/// One `vision.list_models` request/response on a fresh connection; `None`
/// on any I/O, framing or engine error.
async fn list_models(socket_path: &Path) -> Option<Value> {
    let request = Envelope {
        version: PROTOCOL_VERSION,
        kind: "request".to_string(),
        method: methods::LIST_MODELS.to_string(),
        capability: "vision.model.list".to_string(),
        args: Value::Nil,
        request_id: "live-detector".to_string(),
        token: String::new(),
        error: None,
    };
    let frame = request.encode_frame().ok()?;
    let mut stream = UnixStream::connect(socket_path).await.ok()?;
    stream.write_all(&frame).await.ok()?;
    stream.flush().await.ok()?;
    let mut header = [0u8; HEADER_SIZE];
    stream.read_exact(&mut header).await.ok()?;
    let len = decode_len(header, PLUGIN_MAX_FRAME, false).ok()?;
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await.ok()?;
    let reply = Envelope::from_msgpack(&body).ok()?;
    reply.error.is_none().then_some(reply.args)
}

#[cfg(test)]
mod tests;
