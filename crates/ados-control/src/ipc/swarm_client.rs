//! The swarm neighbour-table socket reader.
//!
//! `ados-swarmbus` owns `/run/ados/swarm.sock` and publishes the fleet's neighbour
//! table there as one newline-terminated JSON object at the beacon rate (2 Hz),
//! replaying the last one on connect. This client holds the latest object so
//! `GET /api/swarm/neighbors` and `/api/status/full` read it without blocking.
//!
//! Structurally identical to [`StateIpcClient`](super::state_client) and for the
//! same reasons: an absent socket is normal (a node whose radio has not come up, or
//! a profile that does not run the bus), so the reader logs at debug, retries on a
//! backoff, and the routes degrade rather than fail. It only ever reads.
//!
//! The payload is served through `ados_swarmbus::publish::normalise_payload` rather
//! than re-projected here. That crate owns the published shape and tests it; a
//! second projection on this side is exactly how a producer and a consumer in one
//! build come to disagree about a contract.

use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ados_protocol::ipc::read_newline_line;
use serde_json::Value;
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::sync::oneshot;

/// The socket name the swarm bus binds under the runtime directory.
pub const SWARM_SOCKET_NAME: &str = "swarm.sock";

/// Cap on one published line. The table is bounded (64 entries of ~18 small fields),
/// so this is generous headroom that still stops a runaway producer from growing the
/// read buffer without bound.
const MAX_LINE: usize = 256 * 1024;

/// Reconnect backoff bounds. A missing socket is the common case, so the first retry
/// is quick and the delay grows to a ceiling to avoid spinning.
const BACKOFF_START: Duration = Duration::from_millis(250);
const BACKOFF_MAX: Duration = Duration::from_secs(5);

/// The runtime directory the swarm socket lives under, honouring the `ADOS_RUN_DIR`
/// override the way every other socket path in the agent does.
pub fn default_swarm_socket() -> PathBuf {
    let run_dir = std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string());
    Path::new(&run_dir).join(SWARM_SOCKET_NAME)
}

/// The shared, latest published table. `None` until the first line decodes, which is
/// what the routes render as "the swarm bus is not running".
type Published = Arc<Mutex<Option<Value>>>;

/// Reads the swarm socket and holds the latest published table.
///
/// Cheap to clone (the payload is behind an `Arc`); the route surface holds one in
/// the app state and the background reader holds another.
#[derive(Clone)]
pub struct SwarmIpcClient {
    published: Published,
}

/// A handle that stops the reader task on shutdown and joins it.
pub struct SwarmIpcHandle {
    stop: Option<oneshot::Sender<()>>,
    join: tokio::task::JoinHandle<()>,
}

impl SwarmIpcHandle {
    /// Signal the reader to stop and wait for it to wind down.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        let _ = self.join.await;
    }
}

impl SwarmIpcClient {
    /// Build a client with nothing published and no reader. A route reading this
    /// returns the degraded body.
    pub fn disconnected() -> Self {
        Self {
            published: Arc::new(Mutex::new(None)),
        }
    }

    /// Spawn the background reader against `socket_path`, returning the client paired
    /// with its stop handle.
    pub fn spawn(socket_path: PathBuf) -> (Self, SwarmIpcHandle) {
        let published: Published = Arc::new(Mutex::new(None));
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let join = tokio::spawn(read_loop(socket_path, published.clone(), stop_rx));
        (
            Self { published },
            SwarmIpcHandle {
                stop: Some(stop_tx),
                join,
            },
        )
    }

    /// The latest published table, cloned. `None` until the first line decodes.
    pub fn published(&self) -> Option<Value> {
        self.published.lock().clone()
    }

    /// Overwrite the held payload directly. Test-only seam.
    #[cfg(test)]
    pub fn set_for_test(&self, value: Value) {
        *self.published.lock() = Some(value);
    }
}

/// Connect-then-read-then-reconnect loop, run until the stop signal fires.
async fn read_loop(socket_path: PathBuf, published: Published, stop: oneshot::Receiver<()>) {
    tokio::pin!(stop);
    let mut backoff = BACKOFF_START;
    tracing::info!(path = %socket_path.display(), "swarm client started");
    loop {
        tokio::select! {
            biased;
            _ = &mut stop => {
                tracing::info!("swarm client stopping");
                return;
            }
            connected = UnixStream::connect(&socket_path) => {
                match connected {
                    Ok(stream) => {
                        backoff = BACKOFF_START;
                        tracing::debug!(path = %socket_path.display(), "swarm socket connected");
                        process_stream(BufReader::new(stream), &published, &mut stop).await;
                    }
                    Err(e) => {
                        tracing::debug!(
                            path = %socket_path.display(),
                            error = %e,
                            "swarm socket absent; will retry"
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

/// Read lines from one connected stream until EOF, a read error, or shutdown.
///
/// A single unparseable line is skipped rather than ending the connection: one bad
/// publish must not cost the operator the whole fleet view until the next reconnect.
/// An unrecoverable framing error (a line past the cap with no newline) does end it,
/// because the stream is no longer framable from there.
async fn process_stream<R>(
    mut reader: R,
    published: &Published,
    stop: &mut std::pin::Pin<&mut oneshot::Receiver<()>>,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    loop {
        let line = tokio::select! {
            biased;
            _ = &mut **stop => return,
            r = read_newline_line(&mut reader, MAX_LINE) => r,
        };
        match line {
            Ok(Some(bytes)) => {
                if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                    *published.lock() = Some(value);
                }
            }
            Ok(None) | Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;

    fn sample() -> Value {
        json!({
            "fleet_id": 1,
            "slot": 0,
            "neighbors": [{"slot": 3, "hero": false}],
            "counters": {"beacons_rx": 7, "neighbors_now": 1},
        })
    }

    /// Drive `process_stream` against an in-memory byte source and return what was
    /// held at the end.
    async fn run_against(bytes: Vec<u8>) -> Option<Value> {
        let published: Published = Arc::new(Mutex::new(None));
        let (_tx, rx) = oneshot::channel::<()>();
        tokio::pin!(rx);
        process_stream(Cursor::new(bytes), &published, &mut rx.as_mut()).await;
        let held = published.lock().clone();
        held
    }

    #[tokio::test]
    async fn a_published_line_is_decoded() {
        let mut wire = serde_json::to_vec(&sample()).unwrap();
        wire.push(b'\n');
        assert_eq!(run_against(wire).await, Some(sample()));
    }

    /// The bus publishes at 2 Hz and only the newest table is worth having, so a
    /// burst must leave the LAST one held, not the first.
    #[tokio::test]
    async fn the_newest_of_several_lines_is_held() {
        let mut wire = Vec::new();
        for rx in [1, 2, 9] {
            wire.extend(serde_json::to_vec(&json!({"counters": {"beacons_rx": rx}})).unwrap());
            wire.push(b'\n');
        }
        let got = run_against(wire).await.expect("a table decoded");
        assert_eq!(got["counters"]["beacons_rx"], json!(9));
    }

    /// One bad publish must not cost the operator the fleet view: the line is skipped
    /// and the next good one lands on the same connection.
    #[tokio::test]
    async fn a_malformed_line_is_skipped_and_the_next_good_one_lands() {
        let mut wire = b"{ not json }\n".to_vec();
        wire.extend(serde_json::to_vec(&sample()).unwrap());
        wire.push(b'\n');
        assert_eq!(run_against(wire).await, Some(sample()));
    }

    #[tokio::test]
    async fn nothing_published_leaves_the_cell_empty() {
        assert!(run_against(Vec::new()).await.is_none());
        assert!(SwarmIpcClient::disconnected().published().is_none());
    }

    /// A live round trip over a real Unix socket, including the replay-on-connect the
    /// producer does: the reader must pick up a table published before it connected.
    #[tokio::test]
    async fn the_reader_connects_to_a_real_socket_and_stops_on_its_handle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("swarm.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let payload = sample();
        let served = payload.clone();
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                use tokio::io::AsyncWriteExt;
                let mut line = serde_json::to_vec(&served).unwrap();
                line.push(b'\n');
                let _ = stream.write_all(&line).await;
                let _ = stream.flush().await;
                // Hold the connection open so the reader stays in process_stream.
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });

        let (client, handle) = SwarmIpcClient::spawn(path.clone());
        for _ in 0..100 {
            if client.published().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(client.published(), Some(payload));
        handle.shutdown().await;
    }

    /// An absent socket must leave the reader retrying quietly rather than erroring
    /// out — the steady state on a node whose radio has not come up.
    #[tokio::test]
    async fn an_absent_socket_leaves_the_reader_retrying() {
        let dir = tempfile::tempdir().unwrap();
        let (client, handle) = SwarmIpcClient::spawn(dir.path().join("absent-swarm.sock"));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(client.published().is_none());
        handle.shutdown().await;
    }

    #[test]
    fn the_default_path_resolves_under_the_run_directory() {
        assert!(default_swarm_socket().ends_with("swarm.sock"));
    }
}
