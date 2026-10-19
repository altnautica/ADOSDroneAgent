//! The standalone (`direct`) receive plane: service-wide presence, hop and aux
//! consumers around the per-generation receive loop.

use super::receive::receive_loop;
use super::*;

/// The standalone (`direct`) receive plane.
pub(super) async fn run_direct(
    progress: ados_supervisor::sdnotify::MonitorProgress,
    sigterm: &mut tokio::signal::unix::Signal,
    sigint: &mut tokio::signal::unix::Signal,
) -> Result<()> {
    let config = match wait_for_fleet_identity(&progress, sigterm, sigint).await {
        Some(c) => c,
        None => return Ok(()),
    };
    tracing::info!(
        channel = config.channel,
        band = %config.band,
        interface = %config.interface,
        fleet_id = config.fleet_id,
        "ground-station data-plane starting (direct role)"
    );

    // The fleet slots this ground station receives on, read once for the
    // service. The registry only changes at pair/unpair time, and both paths
    // restart this unit, so a service-scope read is the reconcile point for the
    // consumers that must hold a UDP bind for the whole service lifetime (the
    // presence listener and the per-slot aux consumers). The receive-chain
    // processes themselves are reconciled inside the generation loop, which is
    // free to churn them without dropping a bind.
    let service_slots = wfb_rx::fleet_slots(&FleetRegistry::load(std::path::Path::new(
        FLEET_REGISTRY_PATH,
    )));
    tracing::info!(slots = ?service_slots, "ground_fleet_slots");

    // The presence listen loop + cache run for the whole service lifetime (the
    // listener feeds the per-generation watchdog its peer-presence signal). The
    // emit loop runs service-wide too; both survive receive-plane restarts.
    //
    // The listener runs under a supervisor that re-binds with bounded backoff on
    // a fatal socket error and surfaces a restart counter on a GS sidecar, so a
    // listener fault never permanently freezes the watchdog's presence input.
    let presence_cache = GsPresenceCache::new();
    // Shared radio-group cell, written by the receive loop once it resolves the
    // WFB adapters (transmit adapter first). Created here (before the listener
    // spawn) so the hop follower can retune every adapter on a drone hop; the
    // shutdown path below also restores each one to managed mode.
    let radio_group: wfb_rx::RadioGroup = Arc::new(Mutex::new(Vec::new()));
    // The hop follower retunes the GS receive radios to a drone-announced
    // channel when its countdown expires, so a coordinated hop is a brief
    // retune rather than a blackout the valid-packet watchdog has to sweep out
    // of. It verifies each follow against the receive generation's valid-packet
    // counter, which the receive loop hands it per generation. The group setter
    // keeps every receive adapter on the followed channel.
    let hop_follower = presence::HopFollower::new(
        Arc::new(wfb_rx::GroupChannelSetter::new(
            Arc::new(IwChannelSetter),
            radio_group.clone(),
        )),
        radio_group.clone(),
    );
    tokio::spawn(presence::listen_supervisor(
        presence_cache.clone(),
        Some(hop_follower.clone()),
        service_slots.clone(),
    ));
    {
        // The beacon's channel is a hint; the configured channel is a safe
        // service-wide source (the live channel the watchdog locks is surfaced
        // on the sidecar, not the beacon).
        let beacon_channel = config.channel;
        tokio::spawn(presence::emit_loop(move || beacon_channel));
    }
    // Export the GS-side hop-supervisor snapshot (band + hop-follow history) to
    // /run/ados/hop-supervisor.json so the REST layer + the on-box channel-hops
    // page read the same surface the Python listener produced. Service-wide, so
    // the history survives receive-plane restarts.
    {
        let hop_cache = presence_cache.clone();
        let band = config.band.clone();
        tokio::spawn(presence::hop_supervisor_persist_loop(hop_cache, band));
    }
    // Publish the decoded WFB peers (the drones this ground station relays) to
    // /run/ados/linked-peers.json so the heartbeat can surface `linkedPeers[]`
    // and a GCS paired to the ground node transitively enrols each drone as its
    // own node. Service-wide, so the list survives receive-plane restarts.
    tokio::spawn(presence::linked_peers_persist_loop(presence_cache.clone()));

    // The auxiliary application lane's consumer. The receive chain decodes that
    // lane to a loopback port every generation, but the port is bound once for
    // the service: the generations respawn on link loss, and re-binding a UDP
    // port on that cadence risks losing the bind to its own lingering socket.
    // The counters live at service scope for the same reason and are handed to
    // each generation's stats reader, so the sidecar carries a running total
    // rather than one that resets whenever the video link blinks.
    //
    // Drone MAVLink arriving here is republished onto this node's own MAVLink
    // plane, so a ground control station connected to this ground station sees
    // the vehicle over the ports it already uses.
    //
    // Only one process may hold a UDP bind on the decoded aux port, so anything
    // that later wants a second reader of the lane on this node has to
    // demultiplex it once here and fan out in-process, not bind the port twice.
    let aux_counters = crate::AuxCounters::new();
    let aux_shutdown = Shutdown::new();
    // The relayed-node cache: status and identity frames the linked drone pushes
    // over the lane, held per device id with the age of each. Its sidecar is what
    // lets this node describe what it relays to an operator who is paired only
    // here, and it feeds the peer identity into the linked-peers surface.
    let aux_peers = crate::aux_peers::AuxPeerCache::new();
    // One consumer per registered slot: the receive plane decodes each drone's
    // aux lane to its own loopback egress, so a single bind would hear exactly
    // one drone and every other drone's MAVLink, relayed status and RPC
    // responses would land on a port nobody reads. They share the counters, the
    // peer cache, the MAVLink ingest and the RPC response ingest, all of which
    // are already keyed per device id, so the fleet reports as one aggregate.
    let mavlink_ingest = Arc::new(ados_protocol::mavlink_ingest::MavlinkIngest::at_default_path());
    // The hand-off to the local config-over-radio service. Shared by every slot:
    // config replies correlate by request id inside the tunnel, so one ingress
    // serves the whole fleet the same way the RPC response ingest does. Built
    // unconditionally and harmless when the channel is opted out — nothing is
    // bound to the ingress port then, so a frame is counted and dropped.
    let config_tunnel_ingest =
        Arc::new(ados_protocol::config_tunnel_ingest::ConfigTunnelIngest::at_configured_port());
    // This node's own installed plugins and their contract versions, read once.
    // The identity consumer compares a peer's advertised inventory against this
    // so an upgrade-mixed pair (each half on a different plugin build) is
    // surfaced as a mismatch counter instead of silently disagreeing on the
    // plugin wire.
    let local_plugins: Arc<std::collections::HashMap<String, u16>> = Arc::new(
        ados_plugin_host::inventory::installed_contract_versions_default()
            .into_iter()
            .collect(),
    );
    // The plugin application lane: `radio-aux.sock`, the same socket and
    // protocol the drone's radio service serves. The aux consumers below hand
    // it every decoded AppStream datagram in-process (they already own the
    // decoded ports; nothing binds them twice), and its `send` goes out on the
    // uplink ingress the receive chain's aux `wfb_tx` reads.
    let (app_fanout, _) = tokio::sync::broadcast::channel(crate::app_lane::APP_FANOUT_DEPTH);
    let app_lane = match ados_radio::aux_cmd::AuxCmdState::ground(
        config.aux_tx_port,
        config.aux_enable,
        app_fanout,
    )
    .await
    {
        Ok(state) => Some(state),
        Err(e) => {
            tracing::warn!(error = %e, "ground_app_lane_unavailable");
            None
        }
    };
    let app_lane_task = app_lane.clone().map(|state| {
        let shutdown = aux_shutdown.clone();
        tokio::spawn(async move {
            let path = crate::app_lane::socket_path();
            crate::app_lane::serve(state, &path, shutdown).await
        })
    });
    let spawn_aux_consumer = {
        let aux_counters = aux_counters.clone();
        let aux_peers = aux_peers.clone();
        let aux_shutdown = aux_shutdown.clone();
        // One sink set for every slot: the destinations are process-wide, only
        // the listen port differs per slot.
        let sinks = crate::AuxSinksOwned {
            mavlink: mavlink_ingest.clone(),
            rpc_response: Some(ados_protocol::aux_rpc_proxy::AuxRpcResponseIngest::new(
                ados_protocol::aux_rpc_proxy::DEFAULT_RESPONSE_SOCK,
            )),
            config_tunnel: Some(config_tunnel_ingest.clone()),
            app_stream: app_lane
                .map(|state| Arc::new(state) as Arc<dyn crate::aux_consumer::AppStreamSink>),
            local_plugins: Some(local_plugins.clone()),
        };
        move |slot: u8| -> tokio::task::JoinHandle<()> {
            tokio::spawn(crate::supervise_aux_consumer(
                slot,
                wfb_rx::aux_rx_port(slot),
                sinks.clone(),
                aux_counters.clone(),
                aux_peers.clone(),
                aux_shutdown.clone(),
            ))
        }
    };
    let aux_tasks: Arc<Mutex<std::collections::HashMap<u8, tokio::task::JoinHandle<()>>>> =
        Arc::new(Mutex::new(std::collections::HashMap::new()));
    {
        let mut bound = aux_tasks.lock().await;
        for &slot in &service_slots {
            bound.insert(slot, spawn_aux_consumer(slot));
        }
    }

    // A slot can be issued long after this service started. The pair route
    // deliberately skips re-installing the receive unit when the fleet key is
    // unchanged, which is exactly the normal case for the second and every
    // subsequent drone joining, so those pairings never reach a service restart.
    // The receive plane already reconciles its own processes on this cadence, so
    // without a matching reconcile here a late-joining drone got a `wfb_rx`
    // decoding its lane correctly onto a loopback port with nothing bound to it:
    // its MAVLink, relayed status and every RPC response fragment landed
    // nowhere, and a relay call to it timed out looking exactly like a dead
    // radio.
    //
    // Additive only. An existing consumer is never dropped, so the
    // whole-service-lifetime bind the initial read was written to guarantee is
    // preserved; re-binding a UDP port on the generation cadence is what that
    // design was avoiding. A released slot leaves its consumer parked on a port
    // no one transmits to, which costs nothing and keeps the bind warm if the
    // slot is later reissued.
    let aux_reconcile = {
        let aux_tasks = aux_tasks.clone();
        let spawn_aux_consumer = spawn_aux_consumer.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(FLEET_RECONCILE_INTERVAL);
            tick.tick().await; // the first tick completes immediately
            loop {
                tick.tick().await;
                let want = wfb_rx::fleet_slots(&FleetRegistry::load(std::path::Path::new(
                    FLEET_REGISTRY_PATH,
                )));
                let mut bound = aux_tasks.lock().await;
                let have: std::collections::BTreeSet<u8> = bound.keys().copied().collect();
                for slot in aux_slots_to_bind(&want, &have) {
                    bound.insert(slot, spawn_aux_consumer(slot));
                    tracing::info!(
                        slot,
                        port = wfb_rx::aux_rx_port(slot),
                        "ground_aux_consumer_spawned"
                    );
                }
            }
        })
    };
    let aux_peers_task = tokio::spawn(crate::aux_peers::persist_loop(
        aux_peers.clone(),
        Some(presence_cache.clone()),
        aux_shutdown.clone(),
    ));

    // The adapters are auto-detected inside the receive loop (config's
    // interface is often empty). The shared `radio_group` cell (created above
    // for the hop follower) is the seam the loop writes once it resolves them;
    // on a shutdown signal this side restores each to managed mode, the mirror
    // of the drone-side teardown, so the kernel/NetworkManager can re-enumerate
    // the RTLs instead of finding them stranded in monitor mode after the unit
    // stops.

    // Run the receive loop until a shutdown signal arrives.
    tokio::select! {
        _ = receive_loop(
            &config,
            presence_cache,
            radio_group.clone(),
            hop_follower,
            aux_counters,
            progress,
        ) => {}
        _ = sigterm.recv() => {
            tracing::info!("received SIGTERM");
        }
        _ = sigint.recv() => {
            tracing::info!("received SIGINT");
        }
    }

    // The consumers self-stop on the shared signal; the aborts are no-ops for
    // any that already returned, and reap the ones on the path where no signal
    // fired.
    aux_shutdown.trigger();
    tokio::time::sleep(Duration::from_millis(100)).await;
    // Stop the reconciler before reaping, so it cannot spawn a fresh consumer
    // into the map while the shutdown path is draining it.
    aux_reconcile.abort();
    for t in aux_tasks.lock().await.values() {
        t.abort();
    }
    // The persister waits on the same latching signal and stops on its own;
    // the abort just reaps it without waiting out a write in progress.
    aux_peers_task.abort();
    if let Some(t) = app_lane_task {
        t.abort();
    }

    // Restore the resolved WFB adapters to managed mode on the way out.
    restore_managed_if_resolved(&radio_group).await;
    Ok(())
}

/// The registered slots that have no auxiliary consumer yet.
///
/// Additive by design: a slot that is bound but no longer registered is
/// deliberately left alone rather than torn down. Holding the UDP bind for the
/// whole service lifetime is what keeps a consumer from losing its port to its
/// own lingering socket, and a slot that is later reissued finds its reader
/// already warm.
pub(super) fn aux_slots_to_bind(want: &[u8], bound: &std::collections::BTreeSet<u8>) -> Vec<u8> {
    want.iter()
        .copied()
        .filter(|s| !bound.contains(s))
        .collect()
}

/// Block until `/etc/ados/config.yaml` carries a usable ground-station fleet
/// identity, then return the loaded config. `None` means a shutdown signal
/// arrived while parked.
///
/// A rejected identity is a hard refusal, not a defaulted value: a ground
/// station keyed to a drone slot would share a `channel_id` with that drone and
/// re-init its FEC session roughly once a second (the wfb-ng `Aggregator`
/// re-inits on every foreign session packet), which presents as unexplained
/// link loss rather than as a config fault. Parking and re-reading — rather
/// than exiting — means an identity written by the pair flow is picked up
/// without a service restart.
pub(super) async fn wait_for_fleet_identity(
    progress: &ados_supervisor::sdnotify::MonitorProgress,
    sigterm: &mut tokio::signal::unix::Signal,
    sigint: &mut tokio::signal::unix::Signal,
) -> Option<WfbConfig> {
    let mut logged = false;
    loop {
        // Parked-and-re-reading is progress: this loop is doing exactly its job
        // on an unprovisioned node, and withholding the watchdog here would put
        // a box that is waiting to be paired into a restart loop.
        progress.mark();
        let config = WfbConfig::load_from(std::path::Path::new(CONFIG_YAML));
        let Some(err) = ados_radio::config::fleet_identity_error(
            config.fleet_id,
            config.fleet_slot,
            /* is_ground_station = */ true,
        ) else {
            if logged {
                tracing::info!(
                    fleet_id = config.fleet_id,
                    fleet_slot = config.fleet_slot,
                    "ground_fleet_identity_recovered"
                );
                ados_config::write_config_status("ground_station", None);
            }
            return Some(config);
        };
        // Log + publish once per fault, not once per poll: the sidecar is
        // level-triggered and a 5 s log loop would bury the rest of the journal.
        if !logged {
            tracing::error!(
                fleet_id = config.fleet_id,
                fleet_slot = config.fleet_slot,
                reason = %err,
                "ground_fleet_identity_invalid: receive plane parked"
            );
            ados_config::write_config_status("ground_station", Some(&err.to_string()));
            logged = true;
        }
        tokio::select! {
            _ = sigterm.recv() => return None,
            _ = sigint.recv() => return None,
            _ = tokio::time::sleep(FLEET_RECONCILE_INTERVAL) => {}
        }
    }
}

/// Restore every receive-plane adapter to managed mode on shutdown when the
/// loop resolved any this run. A no-op when it never did (nothing to restore).
/// The read decision is split into [`ifaces_to_restore`] so the
/// capture-then-restore path is unit-testable without a real NIC.
pub(super) async fn restore_managed_if_resolved(group: &wfb_rx::RadioGroup) {
    for iface in ifaces_to_restore(group).await {
        tracing::info!(interface = %iface, "restoring receive adapter to managed mode");
        ados_radio::adapter::set_managed_mode(&iface).await;
    }
}

/// Read the shared radio-group cell and return the interfaces to restore
/// (empty when nothing was resolved). Pure over the cell, so the capture
/// (write from the receive loop) and the read (shutdown side) can be asserted
/// in a unit test without touching a NIC.
pub(super) async fn ifaces_to_restore(group: &wfb_rx::RadioGroup) -> Vec<String> {
    group.lock().await.clone()
}
