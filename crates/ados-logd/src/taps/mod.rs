//! Seam taps: the in-process producers that read the agent's frozen IPC seams
//! and turn what they see into durable rows on the ingest channel.
//!
//! Three taps run as tokio tasks alongside the hardware collector, each owning a
//! clone of the daemon's ingest sender and a subscription to one shared shutdown
//! signal:
//!
//! - [`state`] reads the vehicle-state stream into telemetry metrics plus the
//!   arm/disarm and mode-change events that drive the flight session;
//! - [`mavlink`] samples the raw frame broadcast into a rate-limited diagnostic
//!   trail;
//! - [`sidecar`] tails the runtime JSON sidecars for staleness, drop, and the
//!   scalars worth recording.
//!
//! Each tap consumes its seam and never reimplements it: the taps are not the
//! router, not the supervisor, not the radio. A seam being absent (no agent on a
//! host, an idle agent before a service is up) is normal, and each tap retries
//! on a capped backoff rather than treating absence as an error.

pub mod backoff;
pub mod mavlink;
pub mod sidecar;
pub mod state;

use std::path::PathBuf;

use tokio::sync::{broadcast, mpsc};

use ados_protocol::logd::IngestFrame;

/// The `source` tag on rows produced by the state tap.
pub const SOURCE_STATE: &str = "state-tap";
/// The `source` tag on rows produced by the raw-frame tap.
pub const SOURCE_MAVLINK: &str = "mavlink-tap";
/// The `source` tag on rows produced by the sidecar tailer.
pub const SOURCE_SIDECAR: &str = "sidecar-tap";

/// A shutdown subscription a tap awaits. Each tap holds its own [`Shutdown`];
/// firing the paired [`Shutdowner`] (or dropping it) wakes every subscriber so
/// all taps stop together.
///
/// This wraps a broadcast receiver so one signal fans out to every tap, matching
/// the daemon's symmetric shutdown of the collector and the accept loop.
pub struct Shutdown {
    rx: broadcast::Receiver<()>,
}

impl Shutdown {
    /// Resolve when the paired [`Shutdowner`] fires or is dropped. After the
    /// first resolution it keeps resolving immediately, so a tap that has seen
    /// shutdown once will not block on a later `recv`.
    pub async fn recv(&mut self) {
        // Any outcome (a value, a closed sender, or a lagged receiver) means the
        // tap should stop: there is exactly one logical "stop" message.
        let _ = self.rx.recv().await;
    }

    /// A shutdown that never fires, for driving a tap's processing path to its
    /// natural end (EOF / closed channel) in a test without a stop signal. The
    /// sender is leaked for the process lifetime so the receiver never observes a
    /// closed channel; this is test-only and bounded by the test's own lifetime.
    #[cfg(test)]
    pub fn never() -> Shutdown {
        let (tx, rx) = broadcast::channel(1);
        // Keep the sender alive so `recv` parks forever rather than returning on
        // a closed channel.
        Box::leak(Box::new(tx));
        Shutdown { rx }
    }

    /// Build a `(Shutdowner, Shutdown)` pair for a test that needs to fire the
    /// stop signal explicitly.
    #[cfg(test)]
    pub fn pair() -> (Shutdowner, Shutdown) {
        let (tx, rx) = broadcast::channel(1);
        (Shutdowner { tx }, Shutdown { rx })
    }
}

/// The firing end of a [`Shutdown`]. Calling [`fire`](Shutdowner::fire) (or
/// dropping it) stops every subscribed tap.
pub struct Shutdowner {
    tx: broadcast::Sender<()>,
}

impl Shutdowner {
    /// Signal every subscribed tap to stop.
    pub fn fire(&self) {
        let _ = self.tx.send(());
    }
}

/// Resolved paths the taps read, separate from the daemon's own socket/db paths
/// so they are injectable in a test (a tempdir with fixture files instead of the
/// real runtime directory).
#[derive(Debug, Clone)]
pub struct TapPaths {
    /// The vehicle-state stream socket.
    pub state_socket: PathBuf,
    /// The raw-frame broadcast socket.
    pub mavlink_socket: PathBuf,
    /// The runtime directory the JSON sidecars live under.
    pub sidecar_root: PathBuf,
}

impl Default for TapPaths {
    fn default() -> Self {
        Self {
            state_socket: PathBuf::from(DEFAULT_STATE_SOCKET),
            mavlink_socket: PathBuf::from(DEFAULT_MAVLINK_SOCKET),
            sidecar_root: PathBuf::from(DEFAULT_SIDECAR_ROOT),
        }
    }
}

/// The canonical vehicle-state stream socket.
pub const DEFAULT_STATE_SOCKET: &str = "/run/ados/state.sock";
/// The canonical raw-frame broadcast socket.
pub const DEFAULT_MAVLINK_SOCKET: &str = "/run/ados/mavlink.sock";
/// The canonical runtime directory the JSON sidecars live under.
pub const DEFAULT_SIDECAR_ROOT: &str = "/run/ados";

/// Spawn all three taps as tokio tasks. Each task owns a clone of `ingest_tx`
/// and a fresh [`Shutdown`] subscribed to `shutdownable`, so one fire stops them
/// all. Returns the spawned join handles so the daemon can await them on
/// shutdown, symmetric with how it awaits the collector task.
pub fn spawn_all_taps(
    paths: &TapPaths,
    ingest_tx: mpsc::Sender<IngestFrame>,
    shutdownable: &broadcast::Sender<()>,
    mavlink_sample_hz: f64,
) -> Vec<tokio::task::JoinHandle<()>> {
    let state_tap = tokio::spawn(state::run_state_tap(
        paths.state_socket.clone(),
        ingest_tx.clone(),
        Shutdown {
            rx: shutdownable.subscribe(),
        },
    ));
    let mavlink_tap = tokio::spawn(mavlink::run_mavlink_tap(
        paths.mavlink_socket.clone(),
        ingest_tx.clone(),
        mavlink_sample_hz,
        Shutdown {
            rx: shutdownable.subscribe(),
        },
    ));
    let sidecar_tap = tokio::spawn(sidecar::run_sidecar_tailer(
        paths.sidecar_root.clone(),
        ingest_tx,
        Shutdown {
            rx: shutdownable.subscribe(),
        },
    ));
    vec![state_tap, mavlink_tap, sidecar_tap]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn spawn_all_taps_starts_three_tasks_that_stop_on_one_signal() {
        // Point the taps at a tempdir with no sockets and no sidecars: every tap
        // must come up, sit in its absent-seam backoff, and then stop promptly
        // when the single shutdown signal fires.
        let dir = tempfile::tempdir().unwrap();
        let paths = TapPaths {
            state_socket: dir.path().join("state.sock"),
            mavlink_socket: dir.path().join("mavlink.sock"),
            sidecar_root: dir.path().to_path_buf(),
        };
        let (ingest_tx, _ingest_rx) = mpsc::channel::<IngestFrame>(16);
        let (shutdown_tx, _keep) = broadcast::channel::<()>(1);

        let handles = spawn_all_taps(&paths, ingest_tx, &shutdown_tx, mavlink::DEFAULT_SAMPLE_HZ);
        assert_eq!(handles.len(), 3);

        // Let them attempt their connects / first poll.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // One fire stops all three within the bound.
        let _ = shutdown_tx.send(());
        for h in handles {
            tokio::time::timeout(Duration::from_secs(2), h)
                .await
                .expect("each tap stops within the bound")
                .expect("no tap task panicked");
        }
    }

    #[tokio::test]
    async fn shutdown_never_parks_until_dropped_receiver() {
        // `never()` must not resolve on its own within a short window.
        let mut s = Shutdown::never();
        let r = tokio::time::timeout(Duration::from_millis(100), s.recv()).await;
        assert!(r.is_err(), "never() must not fire on its own");
    }

    #[tokio::test]
    async fn shutdown_pair_fires_the_subscriber() {
        let (stop, mut s) = Shutdown::pair();
        stop.fire();
        tokio::time::timeout(Duration::from_millis(200), s.recv())
            .await
            .expect("fire wakes the subscriber");
    }
}
