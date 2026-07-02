//! The vehicle-state socket reader.
//!
//! The MAVLink service owns `/run/ados/state.sock` and pushes the last-known
//! snapshot on connect, then a fresh one at ~10 Hz. This client connects, decodes
//! each frame, and holds the latest snapshot behind a shared lock so the status
//! and telemetry routes read it without blocking. The frame is self-describing,
//! so the same reader decodes either wire format no matter which the producer is
//! currently emitting:
//!
//! - **v1**: a newline-terminated JSON object, whose first byte is always `{`
//!   (`0x7B`).
//! - **v2**: a 4-byte big-endian length prefix + msgpack body. A snapshot is far
//!   smaller than 16 MB, so the most-significant length byte (the first byte on
//!   the wire) is always `0x00`.
//!
//! Sniffing that first byte means the encoder flag can be flipped across a
//! deployment without lock-stepping every consumer restart. This mirrors the
//! Python `StateIPCClient.read_loop` contract exactly.
//!
//! The socket is absent on a host with no agent, and on an idle or unpaired agent
//! before the state hub comes up. That is normal, not an error: the reader logs
//! the absence at debug level, retries on a backoff, and the snapshot stays
//! `None` so the routes degrade (an empty status, an empty telemetry) rather than
//! fail. The client only ever reads; the state wire model stays frozen.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ados_protocol::state::read_state_value;
use serde_json::Value;
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::sync::oneshot;

/// Default state socket path: the MAVLink service's `/run/ados/state.sock`. The
/// `ADOS_RUN_DIR` env override resolves the runtime root the same way the Python
/// `ados.core.ipc` does, so a test points it at a tempdir and a dev rig can move
/// the whole `/run/ados` tree.
pub const STATE_SOCKET_NAME: &str = "state.sock";

/// The runtime directory the state socket lives under, honouring the
/// `ADOS_RUN_DIR` override (matching the Python `ADOS_RUN_DIR` resolution).
pub fn default_state_socket() -> PathBuf {
    let run_dir = std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string());
    Path::new(&run_dir).join(STATE_SOCKET_NAME)
}

/// Reconnect backoff bounds. A missing socket is the common case (an idle agent),
/// so the first retry is quick and the delay grows to a ceiling to avoid spinning.
const BACKOFF_START: Duration = Duration::from_millis(250);
const BACKOFF_MAX: Duration = Duration::from_secs(5);

/// The shared, latest vehicle-state snapshot. `None` until the first frame
/// decodes (and after a reconnect window where no frame has arrived yet). A route
/// reads it, clones the inner `Value`, and projects the fields it needs.
type Snapshot = Arc<Mutex<Option<Value>>>;

/// Reads the state socket and holds the latest snapshot.
///
/// Cheap to clone (the snapshot is an `Arc`); the route surface holds one in the
/// app state and the background reader task holds another. Build it with
/// [`StateIpcClient::spawn`], which starts the connect-and-read loop; drop the
/// returned client (or call [`StateIpcClient::shutdown`]) to stop the task.
#[derive(Clone)]
pub struct StateIpcClient {
    snapshot: Snapshot,
}

/// A handle that stops the reader task on shutdown and joins it. The daemon holds
/// this for the lifetime of the run; the route surface holds only the cloned
/// [`StateIpcClient`].
pub struct StateIpcHandle {
    stop: Option<oneshot::Sender<()>>,
    join: tokio::task::JoinHandle<()>,
}

impl StateIpcHandle {
    /// Signal the reader to stop and wait for it to wind down.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        let _ = self.join.await;
    }
}

impl StateIpcClient {
    /// Build a client with an empty snapshot and no reader. Used where the
    /// snapshot is fed by a test, or before [`StateIpcClient::spawn`] starts the
    /// loop. A route reading this returns the empty-degraded shape.
    pub fn disconnected() -> Self {
        Self {
            snapshot: Arc::new(Mutex::new(None)),
        }
    }

    /// Spawn the background reader against the given socket path and return the
    /// client paired with its stop handle. The reader connects, decodes snapshots
    /// into the shared cell, and reconnects with backoff on EOF or an absent
    /// socket until the handle is shut down.
    pub fn spawn(socket_path: PathBuf) -> (Self, StateIpcHandle) {
        let snapshot: Snapshot = Arc::new(Mutex::new(None));
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let reader_snapshot = Arc::clone(&snapshot);
        let join = tokio::spawn(read_loop(socket_path, reader_snapshot, stop_rx));
        (
            Self { snapshot },
            StateIpcHandle {
                stop: Some(stop_tx),
                join,
            },
        )
    }

    /// The latest snapshot, cloned. `None` until the first frame decodes; a route
    /// maps that to its empty-degraded shape.
    pub fn snapshot(&self) -> Option<Value> {
        self.snapshot
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Overwrite the held snapshot directly. Test-only seam: a test can prime the
    /// cell without a live socket. Not used on the production read path.
    #[cfg(test)]
    pub fn set_snapshot_for_test(&self, value: Value) {
        *self.snapshot.lock().unwrap_or_else(|p| p.into_inner()) = Some(value);
    }
}

/// Connect-then-read-then-reconnect loop, run on the background task until the
/// stop signal fires. A missing socket backs off and retries; a connected stream
/// is read frame-by-frame into the shared snapshot; an EOF or read error drops
/// back to the reconnect path.
async fn read_loop(socket_path: PathBuf, snapshot: Snapshot, stop: oneshot::Receiver<()>) {
    tokio::pin!(stop);
    let mut backoff = BACKOFF_START;
    tracing::info!(path = %socket_path.display(), "state client started");
    loop {
        tokio::select! {
            biased;
            _ = &mut stop => {
                tracing::info!("state client stopping");
                return;
            }
            connected = UnixStream::connect(&socket_path) => {
                match connected {
                    Ok(stream) => {
                        backoff = BACKOFF_START;
                        tracing::debug!(path = %socket_path.display(), "state socket connected");
                        process_stream(BufReader::new(stream), &snapshot, &mut stop).await;
                        // The stream ended (EOF or error). Loop to reconnect.
                    }
                    Err(e) => {
                        tracing::debug!(
                            path = %socket_path.display(),
                            error = %e,
                            "state socket absent; will retry"
                        );
                        let wait = backoff;
                        backoff = (backoff * 2).min(BACKOFF_MAX);
                        tokio::select! {
                            _ = &mut stop => return,
                            _ = tokio::time::sleep(wait) => {}
                        }
                    }
                }
            }
        }
    }
}

/// Read frames from one connected stream until EOF, a read error, or shutdown,
/// updating the shared snapshot on each decoded frame. The seam is injectable:
/// `reader` is any async byte source, so a test feeds canned frames without a
/// live socket.
async fn process_stream<R>(
    mut reader: R,
    snapshot: &Snapshot,
    stop: &mut std::pin::Pin<&mut oneshot::Receiver<()>>,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    loop {
        let frame = tokio::select! {
            biased;
            _ = &mut **stop => return,
            r = read_state_value(&mut reader) => r,
        };
        match frame {
            Ok(Some(value)) => {
                *snapshot.lock().unwrap_or_else(|p| p.into_inner()) = Some(value);
            }
            Ok(None) | Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::state::{encode_v1, encode_v2};
    use serde_json::json;
    use std::io::Cursor;

    fn sample() -> Value {
        json!({
            "armed": true,
            "mode": "GUIDED",
            "battery": {"voltage": 16.4, "remaining": 87},
            "fc_connected": true,
            "service_uptime": 99.0,
        })
    }

    /// Drive `process_stream` against an in-memory byte source and return the
    /// final held snapshot.
    async fn run_against(bytes: Vec<u8>) -> Option<Value> {
        let snapshot: Snapshot = Arc::new(Mutex::new(None));
        let (_tx, rx) = oneshot::channel::<()>();
        tokio::pin!(rx);
        let reader = Cursor::new(bytes);
        process_stream(reader, &snapshot, &mut rx.as_mut()).await;
        let held = snapshot.lock().unwrap().clone();
        held
    }

    #[tokio::test]
    async fn decodes_a_v1_newline_json_frame() {
        let wire = encode_v1(&sample()).unwrap();
        let got = run_against(wire).await.expect("a snapshot decoded");
        assert_eq!(got, sample());
    }

    #[tokio::test]
    async fn decodes_a_v2_msgpack_frame() {
        let wire = encode_v2(&sample()).unwrap();
        // The v2 frame's leading length byte is 0x00 for a small snapshot.
        assert_eq!(wire[0], 0x00, "small v2 frame must lead with 0x00");
        let got = run_against(wire).await.expect("a snapshot decoded");
        assert_eq!(got, sample());
    }

    #[tokio::test]
    async fn holds_the_latest_of_two_v1_frames() {
        let mut wire = encode_v1(&json!({"mode": "STABILIZE"})).unwrap();
        wire.extend(encode_v1(&json!({"mode": "GUIDED"})).unwrap());
        let got = run_against(wire).await.expect("a snapshot decoded");
        assert_eq!(got["mode"], json!("GUIDED"));
    }

    #[tokio::test]
    async fn a_malformed_v1_line_is_skipped_then_a_good_one_lands() {
        let mut wire = b"{ not json }\n".to_vec();
        wire.extend(encode_v1(&sample()).unwrap());
        let got = run_against(wire).await.expect("the good frame decoded");
        assert_eq!(got, sample());
    }

    #[tokio::test]
    async fn no_frames_leaves_the_snapshot_none() {
        assert!(run_against(Vec::new()).await.is_none());
    }

    #[tokio::test]
    async fn disconnected_client_reads_none() {
        let client = StateIpcClient::disconnected();
        assert!(client.snapshot().is_none());
    }

    #[tokio::test]
    async fn set_snapshot_for_test_is_visible_to_readers() {
        let client = StateIpcClient::disconnected();
        client.set_snapshot_for_test(sample());
        assert_eq!(client.snapshot(), Some(sample()));
    }

    #[tokio::test]
    async fn spawn_reads_a_live_socket_then_stops_on_shutdown() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sock");
        let listener = UnixListener::bind(&path).unwrap();

        let (client, handle) = StateIpcClient::spawn(path.clone());

        // Accept the client's connection and push one snapshot.
        let (mut server, _addr) = listener.accept().await.unwrap();
        let wire = encode_v1(&sample()).unwrap();
        server.write_all(&wire).await.unwrap();
        server.flush().await.unwrap();

        // The reader picks it up within the bound.
        let mut seen = None;
        for _ in 0..50 {
            if let Some(v) = client.snapshot() {
                seen = Some(v);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(seen, Some(sample()));

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn spawn_against_an_absent_socket_holds_none_then_stops() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.sock");
        let (client, handle) = StateIpcClient::spawn(path);
        tokio::time::sleep(Duration::from_millis(60)).await;
        // No socket → no snapshot, and no panic.
        assert!(client.snapshot().is_none());
        handle.shutdown().await;
    }

    #[test]
    fn default_state_socket_honours_the_run_dir_override() {
        // The default path is under the resolved run dir. (Env mutation is
        // process-global; this asserts the join shape, not a specific env.)
        let p = default_state_socket();
        assert!(p.ends_with("state.sock"));
    }
}
