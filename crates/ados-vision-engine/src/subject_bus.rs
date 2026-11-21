//! The `vision-subject.sock` broadcast: every designated-subject state the
//! engine estimates ([`ados_protocol::vision_subject::SubjectState`],
//! `vision.subject.v1`) as one length-prefixed msgpack record, for the
//! services that follow the subject.
//!
//! Last-state: a subscriber that connects while a subject is tracked gets the
//! newest state at once instead of waiting for the next detector batch. A
//! subscriber whose queue fills is dropped by the broadcast server, so the
//! engine never backs up behind it.

use std::sync::Arc;

use ados_protocol::frame::encode_frame;
use ados_protocol::ipc::IpcBroadcast;
use ados_protocol::shutdown::Shutdown;
use ados_protocol::vision_subject::SUBJECT_MAX_BYTES;
use tokio::task::JoinHandle;

use crate::config::VisionConfig;
use crate::engine::VisionEngine;

/// Per-client outbound queue depth: a few seconds of states at detector rate.
const SUBJECT_QUEUE_DEPTH: usize = 64;

/// Serve `vision-subject.sock` in its own task while the tracker runs (only
/// the tracker designates a subject); `None` when it does not.
pub fn spawn(
    engine: &Arc<VisionEngine>,
    config: &VisionConfig,
    cancel: &Shutdown,
) -> Option<JoinHandle<()>> {
    if !config.tracker_enabled {
        return None;
    }
    let (engine, cancel, sock) = (engine.clone(), cancel.clone(), config.subject_socket_path());
    Some(tokio::spawn(async move {
        if let Err(e) = serve(engine, &sock, cancel).await {
            tracing::error!(error = %e, "vision_subject_serve_failed");
        }
    }))
}

/// Bind `vision-subject.sock` and forward every subject state to it until
/// `cancel`.
pub async fn serve(
    engine: Arc<VisionEngine>,
    socket_path: &str,
    cancel: Shutdown,
) -> anyhow::Result<()> {
    let (server, _no_inbound) =
        IpcBroadcast::bind(socket_path, SUBJECT_QUEUE_DEPTH, true, None).await?;
    tracing::info!(path = %socket_path, "vision_subject_sock_listening");
    let mut rx = engine.subscribe_subject();
    loop {
        tokio::select! {
            recv = rx.recv() => match recv {
                Ok(state) => match encode_frame(&state.to_msgpack(), SUBJECT_MAX_BYTES) {
                    Ok(bytes) => server.broadcast(bytes.into()).await,
                    Err(e) => tracing::warn!(error = %e, "vision_subject_encode_failed"),
                },
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            _ = cancel.wait() => break,
        }
    }
    tracing::info!(path = %socket_path, "vision_subject_sock_stopped");
    Ok(())
}
