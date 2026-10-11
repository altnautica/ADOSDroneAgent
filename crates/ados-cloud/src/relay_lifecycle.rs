//! Pairing-aware supervision for the cloud relays.
//!
//! A relay transport is built from the pairing key, so a run built with one key
//! is only valid while that key is current. Each run gets a child shutdown
//! watch: a sibling task polls the pairing key every [`PAIRING_POLL_INTERVAL`]
//! and flips the child when the key differs from the one the run was built
//! with or is gone (unpair). The run and its sibling lanes stop on the child,
//! and the supervisor loops back to its pairing check, so an unpaired node
//! stops relaying and a re-paired node dials with the new key.

use std::future::Future;
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;

/// How often a live relay run re-reads the pairing key (the heartbeat cadence).
pub const PAIRING_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Pause between relay runs, and between pairing checks while unpaired.
pub const RELAY_SETTLE: Duration = Duration::from_secs(5);

/// Spawn the pairing watcher for one relay run. The returned receiver turns
/// `true` when `parent` shuts down, or when `load_key` stops returning
/// `built_with`. Abort the handle once the run has returned.
pub fn spawn_run_shutdown<K>(
    mut parent: watch::Receiver<bool>,
    built_with: String,
    load_key: K,
    poll: Duration,
) -> (watch::Receiver<bool>, JoinHandle<()>)
where
    K: Fn() -> Option<String> + Send + 'static,
{
    let (tx, rx) = watch::channel(false);
    let task = tokio::spawn(async move {
        let mut tick = tokio::time::interval(poll);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // The first tick completes at once; the run was just built with the
        // current key, so the first real check is one poll away.
        tick.tick().await;
        loop {
            if *parent.borrow() {
                let _ = tx.send(true);
                return;
            }
            tokio::select! {
                changed = parent.changed() => {
                    if changed.is_err() || *parent.borrow() {
                        let _ = tx.send(true);
                        return;
                    }
                }
                _ = tick.tick() => {
                    let current = load_key();
                    if current.as_deref() != Some(built_with.as_str()) {
                        tracing::info!(
                            paired = current.is_some(),
                            "cloud relay pairing changed; ending the relay run"
                        );
                        let _ = tx.send(true);
                        return;
                    }
                }
            }
        }
    });
    (rx, task)
}

/// Supervise a relay until `shutdown` turns true. While `load_key` yields a
/// key, `run(key, child_shutdown)` builds one relay run (or `None` when this
/// posture has no broker) and the run is awaited; the child shutdown flips on a
/// pairing change. After each run, or while unpaired, the loop waits `settle`
/// and re-checks.
pub async fn supervise<K, F, Fut>(
    mut shutdown: watch::Receiver<bool>,
    load_key: K,
    poll: Duration,
    settle: Duration,
    mut run: F,
) where
    K: Fn() -> Option<String> + Clone + Send + 'static,
    F: FnMut(String, watch::Receiver<bool>) -> Option<Fut>,
    Fut: Future<Output = ()>,
{
    loop {
        if *shutdown.borrow() {
            break;
        }
        if let Some(key) = load_key() {
            let (child, watcher) =
                spawn_run_shutdown(shutdown.clone(), key.clone(), load_key.clone(), poll);
            if let Some(fut) = run(key, child) {
                fut.await;
            }
            watcher.abort();
        }
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() { break; }
            }
            _ = tokio::time::sleep(settle) => {}
        }
    }
}

#[cfg(test)]
mod tests;
