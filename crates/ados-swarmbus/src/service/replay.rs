//! Replay-window persistence and re-key pruning.

use super::*;

/// Write the replay window to `path` when it changed since the last write. A
/// failed write is logged and left flagged, so the next call retries it.
pub(super) fn persist_replay(path: &Path, table: &Mutex<NeighborTable>) {
    let Some(body) = table.lock().take_unsaved_sender_marks() else {
        return;
    };
    if let Err(e) = ados_protocol::sidecar::write_durable(path, &body) {
        tracing::warn!(path = %path.display(), error = %e, "swarm_replay_state_write_failed");
        table.lock().mark_sender_marks_unsaved();
    }
}

/// Persist the replay window at most once per [`REPLAY_PERSIST_PERIOD`] until
/// `cancel` fires. The fsync runs off the async workers, and each write is
/// awaited before the next tick, so two writes never share the temp file.
pub(super) async fn replay_persist_loop(
    path: std::path::PathBuf,
    table: Arc<Mutex<NeighborTable>>,
    cancel: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            _ = cancelled(&cancel) => return,
            _ = tokio::time::sleep(REPLAY_PERSIST_PERIOD) => {}
        }
        let (path, table) = (path.clone(), table.clone());
        let _ = tokio::task::spawn_blocking(move || persist_replay(&path, &table)).await;
    }
}

/// Drop every neighbour row on a re-key, keeping the counters' history and the
/// replay marks. A row accepted under the old key (the publicly derivable
/// cold-start key, before a bind) must not be published under the new key's
/// `fleet_key_bound` flag and steered on until it ages out; the table refills
/// from beacons sealed with the new key. The replay marks stay, so a captured
/// old beacon cannot be replayed into the fresh table. Returns how many rows
/// were dropped (counted as stale drops).
pub(super) fn forget_neighbors_on_rekey(table: &mut NeighborTable, now: Instant) -> usize {
    // Every row is stale relative to an instant far past the staleness window.
    table.prune(now + Duration::from_secs(3600))
}
