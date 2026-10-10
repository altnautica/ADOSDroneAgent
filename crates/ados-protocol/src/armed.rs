//! Whether the vehicle counts as armed, read from the router's state socket.
//!
//! Every consumer that must refuse a link-dropping action in flight (the
//! control surface's interlock, the plugin host's unattended updates, the
//! installer and the CLI) answers the same question the same way: the latest
//! vehicle-state snapshot says `armed: false` and is younger than
//! [`ARMED_STATE_MAX_AGE`], or the vehicle is treated as armed. No state, a
//! stale state, or a state without an `armed` field proves nothing, so each of
//! those fails closed.
//!
//! [`StateSocketProbe`] keeps a live snapshot for a long-running service;
//! [`read_state_snapshot`] reads one snapshot for a one-shot caller.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::io::BufReader;
use tokio::net::UnixStream;

use crate::retry::{RetryPace, OS_STATE};
use crate::state::read_state_value;

/// How old the vehicle state may be before the vehicle is assumed armed.
pub const ARMED_STATE_MAX_AGE: Duration = Duration::from_secs(3);

/// Reconnect pacing for the probe: fixed, uncapped.
const RECONNECT_PACE: RetryPace = OS_STATE;

/// Whether the vehicle should be treated as armed: the latest state says so,
/// or there is no state newer than [`ARMED_STATE_MAX_AGE`] to say otherwise.
pub fn vehicle_treated_as_armed(snapshot: Option<(Instant, Value)>) -> bool {
    match snapshot {
        Some((at, state)) if at.elapsed() <= ARMED_STATE_MAX_AGE => {
            state.get("armed").and_then(Value::as_bool) != Some(false)
        }
        _ => true,
    }
}

/// A source of the armed verdict.
pub trait ArmedProbe: Send + Sync {
    /// Whether a link-dropping action must be refused right now.
    fn treated_as_armed(&self) -> bool;
}

/// An [`ArmedProbe`] fed by a background reader of the router state socket.
pub struct StateSocketProbe {
    latest: Mutex<Option<(Instant, Value)>>,
    vehicle_profile: bool,
}

impl StateSocketProbe {
    /// Start the reader task on the current tokio runtime and return the probe.
    /// A node whose profile carries no vehicle never counts as armed; the task
    /// is not started for it. The task reconnects every 2 s, with no cap, after
    /// a connect or read failure, and ends when the last probe handle drops.
    pub fn spawn(state_socket: PathBuf, vehicle_profile: bool) -> Arc<Self> {
        let probe = Arc::new(Self {
            latest: Mutex::new(None),
            vehicle_profile,
        });
        if vehicle_profile {
            let weak = Arc::downgrade(&probe);
            tokio::spawn(async move { read_loop(state_socket, weak).await });
        }
        probe
    }

    fn store(&self, value: Value) {
        if let Ok(mut latest) = self.latest.lock() {
            *latest = Some((Instant::now(), value));
        }
    }

    fn latest(&self) -> Option<(Instant, Value)> {
        self.latest.lock().ok().and_then(|latest| latest.clone())
    }
}

impl ArmedProbe for StateSocketProbe {
    fn treated_as_armed(&self) -> bool {
        self.vehicle_profile && vehicle_treated_as_armed(self.latest())
    }
}

async fn read_loop(state_socket: PathBuf, probe: std::sync::Weak<StateSocketProbe>) {
    loop {
        if probe.strong_count() == 0 {
            return;
        }
        match UnixStream::connect(&state_socket).await {
            Ok(stream) => {
                let mut reader = BufReader::new(stream);
                loop {
                    match read_state_value(&mut reader).await {
                        Ok(Some(value)) => match probe.upgrade() {
                            Some(p) => p.store(value),
                            None => return,
                        },
                        Ok(None) => break,
                        Err(e) => {
                            tracing::debug!(error = %e, "armed probe: state read failed");
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                tracing::debug!(
                    path = %state_socket.display(),
                    error = %e,
                    "armed probe: state socket absent; will retry"
                );
            }
        }
        tokio::time::sleep(RECONNECT_PACE.wait()).await;
    }
}

/// Read one vehicle-state snapshot from `state_socket`, giving up after
/// `timeout`. `None` when the socket is absent, the stream ends, or no frame
/// arrives in time; pass the result to [`vehicle_treated_as_armed`].
pub async fn read_state_snapshot(
    state_socket: &Path,
    timeout: Duration,
) -> Option<(Instant, Value)> {
    let read = async {
        let stream = UnixStream::connect(state_socket).await.ok()?;
        let mut reader = BufReader::new(stream);
        read_state_value(&mut reader).await.ok().flatten()
    };
    let value = tokio::time::timeout(timeout, read).await.ok()??;
    Some((Instant::now(), value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_armed_unknown_or_stale_vehicle_counts_as_armed() {
        let now = Instant::now();
        assert!(vehicle_treated_as_armed(None), "no state fails closed");
        assert!(vehicle_treated_as_armed(Some((
            now,
            json!({"armed": true})
        ))));
        assert!(!vehicle_treated_as_armed(Some((
            now,
            json!({"armed": false})
        ))));
        assert!(vehicle_treated_as_armed(Some((now, json!({})))));
        let stale = now - ARMED_STATE_MAX_AGE - Duration::from_secs(1);
        assert!(
            vehicle_treated_as_armed(Some((stale, json!({"armed": false})))),
            "state older than the bound proves nothing"
        );
    }

    #[tokio::test]
    async fn a_probe_without_a_vehicle_never_counts_as_armed() {
        let dir = tempfile::tempdir().unwrap();
        let probe = StateSocketProbe::spawn(dir.path().join("state.sock"), false);
        assert!(!probe.treated_as_armed());
    }

    #[tokio::test]
    async fn a_vehicle_probe_with_no_state_counts_as_armed() {
        let dir = tempfile::tempdir().unwrap();
        let probe = StateSocketProbe::spawn(dir.path().join("state.sock"), true);
        assert!(probe.treated_as_armed());
        probe.store(json!({"armed": false}));
        assert!(!probe.treated_as_armed());
    }

    #[tokio::test]
    async fn a_missing_socket_reads_no_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let snap =
            read_state_snapshot(&dir.path().join("state.sock"), Duration::from_millis(200)).await;
        assert!(snap.is_none());
    }
}
