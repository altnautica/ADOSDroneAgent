//! The `state.sock` snapshot reader the bridge folds into its heartbeat.

use super::*;

/// A live vehicle-state reader over the state IPC socket (`/run/ados/state.sock`).
/// Connects on demand, reads the newest state snapshot (auto-detecting the v1
/// newline-JSON and v2 length-prefixed msgpack wire forms per frame), and caches
/// it for the heartbeat. The reader holds the latest snapshot behind a mutex so
/// the synchronous [`StateSnapshotSource::latest`] never blocks the heartbeat tick.
pub struct StateIpcReader {
    pub(super) latest: std::sync::Arc<std::sync::Mutex<Option<serde_json::Value>>>,
}

impl StateIpcReader {
    /// Build a reader and spawn its background poll against `path`
    /// (`/run/ados/state.sock` by default). The poll reconnects on any read
    /// error; absence of the socket simply leaves `latest` empty.
    pub fn spawn(path: PathBuf, mut shutdown: watch::Receiver<bool>) -> Self {
        let latest = std::sync::Arc::new(std::sync::Mutex::new(None));
        let store = latest.clone();
        tokio::spawn(async move {
            loop {
                if *shutdown.borrow() {
                    return;
                }
                match tokio::net::UnixStream::connect(&path).await {
                    Ok(stream) => {
                        let mut reader = tokio::io::BufReader::new(stream);
                        loop {
                            tokio::select! {
                                _ = shutdown.changed() => { if *shutdown.borrow() { return; } }
                                read = ados_protocol::state::read_state_value(&mut reader) => {
                                    match read {
                                        // One decoded snapshot (v1 newline-JSON or
                                        // v2 length-prefixed msgpack): cache it.
                                        Ok(Some(v)) => {
                                            if let Ok(mut g) = store.lock() {
                                                *g = Some(v);
                                            }
                                        }
                                        // Clean EOF, or an unrecoverable
                                        // framing/IO error → reconnect.
                                        Ok(None) | Err(_) => break,
                                    }
                                }
                            }
                        }
                        // The router is gone: there is no current vehicle
                        // state, so the last snapshot must not keep being
                        // reported as live.
                        if let Ok(mut g) = store.lock() {
                            *g = None;
                        }
                        // A fixed pause so an accept-then-close flap cannot spin.
                        tokio::select! {
                            _ = shutdown.changed() => { if *shutdown.borrow() { return; } }
                            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                        }
                    }
                    Err(_) => {
                        // No socket yet; wait and retry.
                        tokio::select! {
                            _ = shutdown.changed() => { if *shutdown.borrow() { return; } }
                            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                        }
                    }
                }
            }
        });
        Self { latest }
    }
}

impl StateSnapshotSource for StateIpcReader {
    fn latest(&self) -> Option<serde_json::Value> {
        self.latest.lock().ok().and_then(|g| g.clone())
    }
}
