//! The `pilot.v1` socket server.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use ados_protocol::pilot::{self, Request};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::core::Reply;

/// A request for the core task, with the slot for its reply.
pub type CoreReq = (Request, oneshot::Sender<Reply>);

/// The setup engine's entry point for `setup.*` ops. `sink` reaches the
/// requesting client, for the 5 Hz step stream of `setup.subscribe`.
pub trait SetupHandler: Send + Sync + 'static {
    fn handle(
        &self,
        op: String,
        body: rmpv::Value,
        sink: mpsc::Sender<Vec<u8>>,
    ) -> Pin<Box<dyn Future<Output = rmpv::Value> + Send>>;
}

/// Shared by every client connection.
#[derive(Clone)]
pub struct Shared {
    pub core: mpsc::Sender<CoreReq>,
    /// Encoded server messages for subscribers (state, obstacles, sticks,
    /// lease.revoked).
    pub publish: broadcast::Sender<Arc<Vec<u8>>>,
    pub setup: Option<Arc<dyn SetupHandler>>,
}

/// Bind `path` (replacing a stale socket) and serve until the task is dropped.
pub async fn serve(path: &Path, shared: Shared) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    set_mode(path);
    tracing::info!(path = %path.display(), "pilot_socket_listening");
    loop {
        let (stream, _) = listener.accept().await?;
        let shared = shared.clone();
        tokio::spawn(async move {
            if let Err(e) = client(stream, shared).await {
                tracing::debug!(error = %e, "pilot_client_closed");
            }
        });
    }
}

fn set_mode(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660)) {
        tracing::warn!(error = %e, "pilot_socket_chmod_failed");
    }
}

async fn client(stream: UnixStream, shared: Shared) -> Result<(), pilot::PilotWireError> {
    let (mut rd, mut wr) = stream.into_split();
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(256);
    let writer = tokio::spawn(async move {
        while let Some(body) = out_rx.recv().await {
            if pilot::write_msg(&mut wr, &body).await.is_err() {
                break;
            }
        }
    });
    let mut subscribed = false;
    let result = loop {
        let body = match pilot::read_msg(&mut rd).await {
            Ok(Some(b)) => b,
            Ok(None) => break Ok(()),
            Err(e) => break Err(e),
        };
        let req = match pilot::decode_request(&body) {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!(error = %e, "pilot_request_rejected");
                continue;
            }
        };
        match req {
            Request::Subscribe => {
                if !subscribed {
                    subscribed = true;
                    spawn_forwarder(shared.publish.subscribe(), out_tx.clone());
                }
            }
            Request::Other { op, body } if op.starts_with("setup.") => {
                let reply = match &shared.setup {
                    Some(h) => h.handle(op.clone(), body, out_tx.clone()).await,
                    None => error_map("setup_unavailable"),
                };
                if let Ok(bytes) = pilot::encode_op(&op, &reply) {
                    let _ = out_tx.send(bytes).await;
                }
            }
            Request::Other { op, .. } => {
                tracing::debug!(op, "pilot_unknown_op");
            }
            req => {
                let op = op_name(&req);
                let action = matches!(req, Request::Action(_));
                let (tx, rx) = oneshot::channel();
                if shared.core.send((req, tx)).await.is_err() {
                    break Ok(());
                }
                let out = out_tx.clone();
                let answer = async move {
                    let encoded = match rx.await {
                        Ok(Reply::Lease(l)) => pilot::encode_op(op, &l).ok(),
                        Ok(Reply::Action(a)) => pilot::encode_op("action_result", &a).ok(),
                        _ => None,
                    };
                    if let Some(b) = encoded {
                        let _ = out.send(b).await;
                    }
                };
                // An action may wait on the FC (Take control): its answer
                // never holds up the sticks and heartbeats behind it.
                if action {
                    tokio::spawn(answer);
                } else {
                    answer.await;
                }
            }
        }
    };
    drop(out_tx);
    let _ = writer.await;
    result
}

fn op_name(req: &Request) -> &'static str {
    match req {
        Request::LeaseAcquire { .. } => "lease.acquire",
        _ => "reply",
    }
}

fn error_map(err: &str) -> rmpv::Value {
    rmpv::Value::Map(vec![
        (rmpv::Value::from("ok"), rmpv::Value::from(false)),
        (rmpv::Value::from("err"), rmpv::Value::from(err)),
    ])
}

fn spawn_forwarder(mut rx: broadcast::Receiver<Arc<Vec<u8>>>, out: mpsc::Sender<Vec<u8>>) {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(msg) => {
                    // A slow subscriber drops messages rather than stalling the core.
                    if out.try_send(msg.as_ref().clone()).is_err() && out.is_closed() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::debug!(skipped = n, "pilot_subscriber_lagged");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}
