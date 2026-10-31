//! Receive, transmit and publish loops.

use super::*;

/// Fold every captured frame into the table until the socket fails.
pub(super) async fn recv_loop(bus: Arc<SwarmBus>, table: Arc<Mutex<NeighborTable>>) {
    loop {
        match bus.recv_into(&table).await {
            Ok(Ingest::Rejected(reason)) => {
                tracing::trace!(?reason, "swarm frame rejected");
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e, "swarm_radio_recv_failed");
                return;
            }
        }
    }
}

/// Broadcast this node's beacon at the jittered beacon rate. Returns after
/// [`TX_FAILURE_LIMIT`] consecutive injection failures so the supervisor reopens
/// the radio; an isolated failure (a full driver queue) is only a dropped beacon.
pub(super) async fn transmit_loop(
    bus: Arc<SwarmBus>,
    slot: u8,
    table: Arc<Mutex<NeighborTable>>,
    state: SharedState,
) {
    let started = Instant::now();
    let mut failures = TxFailures::default();
    loop {
        // Fresh jitter every transmission, not once at startup: a fleet powered up
        // together must not stay in lockstep.
        tokio::time::sleep(beacon_delay(random_word())).await;
        // A snapshot whose ARRIVAL is older than the window is not this node's
        // state at all: the beacon carries only the slot. A live snapshot whose FC
        // POSITION FIX is older than the window keeps its armed, mode, hero,
        // emergency and precedence readings but radiates no position, velocity or
        // GPS_OK, because every receiving drone dead-reckons those FORWARD from
        // the moment the beacon arrives: a frozen fix would read across the fleet
        // as a node still flying on a track it left behind.
        let held = state.lock().clone();
        let now = Instant::now();
        if let Some(snapshot) = held.as_ref() {
            if !beacon_body(Some(snapshot), now).is_some_and(|(_, fix_trusted)| fix_trusted) {
                // Which age tripped, because they mean different faults: a
                // stalled router versus a live router publishing a dead fix.
                tracing::debug!(
                    arrival_age_ms =
                        now.saturating_duration_since(snapshot.received).as_millis() as u64,
                    fix_age_ms = fix_age(snapshot).map(|d| d.as_millis() as u64),
                    "swarm_beacon_own_state_stale"
                );
            }
        }
        // Sender uptime, truncated to 16 bits. It wraps every 65.5 s, which is far
        // longer than the staleness window it feeds.
        let seq_ms = started.elapsed().as_millis() as u16;
        let beacon = own_beacon(held.as_ref(), now, slot, seq_ms);
        match bus.broadcast(&beacon).await {
            Ok(()) => {
                table.lock().record_tx();
                failures.record(true);
            }
            Err(e) => {
                tracing::debug!(error = %e, "swarm_beacon_tx_failed");
                if failures.record(false) {
                    tracing::warn!(
                        error = %e,
                        failures = TX_FAILURE_LIMIT,
                        "swarm_beacon_tx_failing"
                    );
                    return;
                }
            }
        }
    }
}

/// Prune and publish the table at the beacon rate.
pub(super) async fn publish_loop(
    cfg: SwarmBusConfig,
    table: Arc<Mutex<NeighborTable>>,
    publisher: Arc<IpcBroadcast>,
    cancel: watch::Receiver<bool>,
) {
    let mut device_ids = load_device_ids(Path::new(FLEET_REGISTRY_PATH));
    let mut last_registry_read = Instant::now();
    loop {
        tokio::select! {
            _ = cancelled(&cancel) => return,
            _ = tokio::time::sleep(BEACON_PERIOD) => {}
        }
        if last_registry_read.elapsed() >= REGISTRY_REFRESH {
            device_ids = load_device_ids(Path::new(FLEET_REGISTRY_PATH));
            last_registry_read = Instant::now();
        }
        let now = Instant::now();
        let payload = {
            let mut guard = table.lock();
            // Prune before publishing so a consumer never sees an entry the table
            // has already decided is dead.
            guard.prune(now);
            neighbors_payload(cfg.fleet_id, &guard, &device_ids, now)
        };
        publisher.broadcast(encode_line(&payload).into()).await;
    }
}
