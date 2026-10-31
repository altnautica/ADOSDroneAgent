//! Radio open, reopen and interface resolution.

use super::*;

/// Open the radio, run the transmit and receive loops on it, and reopen it if it
/// fails or the fleet key changes.
///
/// The key file is re-read every [`REGISTRY_REFRESH`]. A bind or a pair rewrites
/// it under a running service, and a bus left on the old key would count every
/// re-keyed peer as a bad tag while they counted it the same way. On a change the
/// cipher is rebuilt with [`SwarmCipher::rekeyed`], which keeps this node's nonce
/// prefix and counter, and the bus is reopened on it.
pub(super) async fn radio_supervisor(
    cfg: SwarmBusConfig,
    table: Arc<Mutex<NeighborTable>>,
    state: SharedState,
    cancel: watch::Receiver<bool>,
) {
    // A key file that exists but cannot be read refuses the bus: it is not
    // started on the cold-start constant. Re-checked until it reads cleanly.
    let mut keys = loop {
        if let Some(k) = FleetKeyWatch::new(ados_radio::paths::DRONE_KEY) {
            break k;
        }
        tracing::error!("swarm_bus_refused: key_unavailable");
        tokio::select! {
            _ = cancelled(&cancel) => return,
            _ = tokio::time::sleep(REGISTRY_REFRESH) => {}
        }
    };
    let mut cipher = Arc::new(SwarmCipher::new(keys.key()));
    // The prefix survives every re-key below, so recording it once is enough.
    table.lock().set_own_sender(cipher.sender_prefix());
    loop {
        // A key that changed while the radio was down is picked up before reopening.
        if let Some(key) = keys.poll() {
            cipher = Arc::new(cipher.rekeyed(&key));
            forget_neighbors_on_rekey(&mut table.lock(), Instant::now());
        }
        let bus = match open_bus(&cfg, &cipher, &cancel).await {
            Some(b) => Arc::new(b),
            None => return,
        };
        tracing::info!(
            iface = bus.iface(),
            fleet_id = bus.fleet_id(),
            slot = bus.slot(),
            "swarm bus open"
        );
        {
            let mut t = table.lock();
            t.set_radio_iface(Some(bus.iface().to_string()));
            // Re-stated on every open: a re-key from the cold-start key to a
            // bound one reopens the bus through here.
            t.set_fleet_key_bound(keys.bound());
        }

        let mut rx = tokio::spawn(recv_loop(bus.clone(), table.clone()));
        // A ground station receives only. Slot 0 is not an aircraft, so it has no
        // position to broadcast and must never appear in a neighbour table.
        let mut tx = (!cfg.is_ground_station()).then(|| {
            tokio::spawn(transmit_loop(
                bus.clone(),
                cfg.fleet_slot,
                table.clone(),
                state.clone(),
            ))
        });

        let outcome = loop {
            tokio::select! {
                _ = cancelled(&cancel) => break Reopen::Stop,
                // The receive loop only returns on a socket error, which means the
                // adapter went away. Drop both loops and reopen.
                _ = &mut rx => break Reopen::RadioLost,
                // The transmit loop returns only after a run of injection
                // failures: the receive socket may still be bound, but this node
                // has stopped radiating, so reopen the whole bus.
                _ = transmit_ended(&mut tx) => {
                    tx = None;
                    break Reopen::TxFailed;
                }
                _ = tokio::time::sleep(REGISTRY_REFRESH) => {
                    if let Some(key) = keys.poll() {
                        cipher = Arc::new(cipher.rekeyed(&key));
                        break Reopen::Rekeyed;
                    }
                }
            }
        };
        table.lock().set_radio_iface(None);
        rx.abort();
        if let Some(tx) = tx {
            // Wait the transmitter out, so the old cipher seals nothing after the
            // rebuilt one took over its counter.
            tx.abort();
            let _ = tx.await;
        }
        match outcome {
            Reopen::Stop => return,
            Reopen::Rekeyed => {
                let dropped = forget_neighbors_on_rekey(&mut table.lock(), Instant::now());
                tracing::info!(dropped, "swarm fleet key changed; reopening the bus");
            }
            Reopen::RadioLost | Reopen::TxFailed => {
                if matches!(outcome, Reopen::RadioLost) {
                    tracing::warn!("swarm radio receive ended; reopening");
                } else {
                    tracing::warn!("swarm beacon injection keeps failing; reopening");
                }
                tokio::select! {
                    _ = cancelled(&cancel) => return,
                    _ = tokio::time::sleep(RADIO_RETRY) => {}
                }
            }
        }
    }
}

/// Resolves when the transmit task ends; never, on a node that runs none.
pub(super) async fn transmit_ended(tx: &mut Option<tokio::task::JoinHandle<()>>) {
    match tx {
        Some(handle) => {
            let _ = handle.await;
        }
        None => std::future::pending().await,
    }
}

impl TxFailures {
    /// Record one send outcome; `true` once the run of failures reaches
    /// [`TX_FAILURE_LIMIT`] and the transmitter should give the radio up.
    pub(super) fn record(&mut self, sent: bool) -> bool {
        if sent {
            self.consecutive = 0;
            return false;
        }
        self.consecutive += 1;
        self.consecutive >= TX_FAILURE_LIMIT
    }
}

/// Resolve an interface and open the bus, retrying until it works or `cancel`
/// fires.
///
/// The loopback transport needs no monitor interface, so it opens with
/// [`LOOPBACK_IFACE`] when none is configured or reported.
pub(super) async fn open_bus(
    cfg: &SwarmBusConfig,
    cipher: &Arc<SwarmCipher>,
    cancel: &watch::Receiver<bool>,
) -> Option<SwarmBus> {
    loop {
        match Transport::from_env() {
            Err(e) => tracing::error!(error = %e, "swarm_transport_invalid"),
            Ok(transport) => {
                let iface = resolve_interface(cfg)
                    .or_else(|| transport.is_loopback().then(|| LOOPBACK_IFACE.to_string()));
                match iface {
                    Some(iface) => match SwarmBus::open_on(
                        &transport,
                        &iface,
                        cfg.fleet_id,
                        cfg.fleet_slot,
                        cipher.clone(),
                    ) {
                        Ok(bus) => return Some(bus),
                        Err(e) => tracing::warn!(
                            %iface, loopback = transport.is_loopback(), error = %e,
                            "swarm_radio_open_failed: is the adapter in monitor mode or the relay up?"
                        ),
                    },
                    None => tracing::debug!(
                        "swarm_radio_interface_unknown: waiting for the radio manager"
                    ),
                }
            }
        }
        tokio::select! {
            _ = cancelled(cancel) => return None,
            _ = tokio::time::sleep(RADIO_RETRY) => {}
        }
    }
}

/// The monitor interface to inject on: the operator's config pin first, then the
/// live selection from the radio service's sidecar.
///
/// The sidecar is the authoritative source in practice — `video.wfb.interface` is
/// usually empty, and the adapter the radio manager actually selected is the only
/// one carrying this fleet's traffic. Injecting on a different interface would
/// produce a bus nobody hears, with no error anywhere.
pub fn resolve_interface(cfg: &SwarmBusConfig) -> Option<String> {
    if !cfg.interface.trim().is_empty() {
        return Some(cfg.interface.trim().to_string());
    }
    interface_from_sidecar(&ados_protocol::paths::run_path("wfb-stats"))
}

/// Read the live interface out of the radio service's `wfb-stats.json`.
pub fn interface_from_sidecar(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let iface = v.get("interface")?.as_str()?.trim();
    (!iface.is_empty()).then(|| iface.to_string())
}
