//! Entry point for the ground-station data-plane service.
//!
//! Dispatches on the mesh role: `direct` runs the standalone WFB receive
//! manager (this file's `receive_loop`); `relay` forwards drone fragments to a
//! receiver over batman-adv; `receiver` aggregates the local NIC + remote relay
//! forwards and republishes the combined FEC stream. The role comes from the
//! `--role` argument when present, else the `/etc/ados/mesh/role` sentinel
//! (`role_manager` owns that file). The relay/receiver roles run as their own
//! systemd units (`ados-wfb-relay` / `ados-wfb-receiver`), each invoking this
//! binary with the matching `--role`.
//!
//! Direct-role detail: per generation it spawns the data RX + both control
//! planes, starts the video fan-out and the presence emit/listen loops as
//! sub-services, and runs the stats reader, the valid-packet watchdog, and the
//! stdout-silence zombie watchdog concurrently. When the data RX exits (or a
//! watchdog terminates it), the generation ends and the loop respawns with a
//! bounded backoff.
//!
//! Adapter detection for the direct receive plane takes the already-prepared
//! interface from config; the relay/receiver roles run the shared radio
//! selector themselves (adapter detect + monitor mode) before spawning their
//! forwarder/aggregator. The rx-key pairing gate and regulatory-domain/tx-power
//! application stay where they were.

use std::sync::atomic::AtomicU32;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{Mutex, Notify};

use ados_radio::config::WfbConfig;
use ados_radio::link_quality::LinkStats;

use ados_groundlink::wfb_rx::{
    self, DataRxHandle, IwChannelSetter, SharedValidCounter, SlotReceivers, SystemClock,
    WfbRxManager,
};
use ados_groundlink::{
    fanout, mesh, presence, receiver, relay, FleetRegistry, GsPresenceCache,
    FLEET_RECONCILE_INTERVAL, FLEET_REGISTRY_PATH,
};

const CONFIG_YAML: &str = "/etc/ados/config.yaml";
const RX_KEY: &str = ados_radio::paths::WFB_RX_KEY;

/// How often the unpaired-state sidecar is refreshed while the pairing gate
/// blocks. Comfortably inside every reader's staleness window, and far below the
/// 5 s poll cadence, so an unpaired ground station stays visible without writing
/// to the card twelve times a minute.
const UNPAIRED_SIDECAR_REFRESH: Duration = Duration::from_secs(20);

/// The run role the service dispatches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Direct,
    Relay,
    Receiver,
}

impl Role {
    /// Parse a role token, returning `None` for anything that is not one of the
    /// three known values (so the caller can distinguish "unknown" from a real
    /// role and log accordingly).
    fn from_token(token: &str) -> Option<Self> {
        match token {
            "direct" => Some(Self::Direct),
            "relay" => Some(Self::Relay),
            "receiver" => Some(Self::Receiver),
            _ => None,
        }
    }
}

/// Resolve the run role: an explicit `--role <value>` argument wins, else the
/// on-disk sentinel. Unknown values fall back to `direct`.
fn resolve_role() -> Role {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let sentinel = mesh::get_current_role();
    resolve_role_from(&args, Some(sentinel.as_str()))
}

/// Pure role-resolution core (test seam, mirroring the `emit`/`emit_to` split in
/// the mesh-event module). An explicit `--role <value>` / `--role=<value>`
/// argument wins; an unknown explicit value is warned and the resolution falls
/// through to the sentinel; with no argument the on-disk sentinel decides; with
/// neither a usable argument nor a usable sentinel the role is `direct`.
fn resolve_role_from(args: &[String], sentinel: Option<&str>) -> Role {
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--role" {
            if let Some(v) = it.next() {
                if let Some(role) = Role::from_token(v) {
                    return role;
                }
                tracing::warn!(value = %v, "unknown_role_arg_falling_back");
            }
        } else if let Some(v) = arg.strip_prefix("--role=") {
            if let Some(role) = Role::from_token(v) {
                return role;
            }
            tracing::warn!(value = %v, "unknown_role_arg_falling_back");
        }
    }
    sentinel.and_then(Role::from_token).unwrap_or(Role::Direct)
}

fn init_logging() {
    use ados_protocol::logd::layer::LogdLayer;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::EnvFilter;

    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());

    // The logd layer ships records to the logging daemon's ingest socket
    // alongside the primary sink; it is best-effort and never blocks the service.
    #[cfg(target_os = "linux")]
    {
        if let Ok(journald) = tracing_journald::layer() {
            let _ = tracing_subscriber::registry()
                .with(EnvFilter::new(&filter))
                .with(journald)
                .with(LogdLayer::new("ados-groundlink"))
                .try_init();
            return;
        }
    }

    let _ = tracing_subscriber::registry()
        .with(EnvFilter::new(&filter))
        .with(tracing_subscriber::fmt::layer())
        .with(LogdLayer::new("ados-groundlink"))
        .try_init();
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();

    // Publish this service's config-status sidecar so a malformed `ground_station:`
    // config block surfaces on the remote Health view, not just in the log. Read
    // once at startup; the role loops re-read the (unchanged) file as they consume
    // it. Best-effort — never blocks startup.
    ados_groundlink::GroundStationConfig::publish_config_status(std::path::Path::new(CONFIG_YAML));

    // Tell systemd we are up (reuses the orchestrator's notify shim).
    ados_supervisor::sdnotify::ready();

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    // The operator command socket runs for the whole service lifetime in every
    // role: role transitions, gateway-preference, and WFB pair-key install /
    // unpair are operator on-demand actions the native front forwards here (it
    // has no in-process Python pair/role manager to call). Spawned before the
    // role dispatch so it is reachable regardless of which role loop runs below.
    tokio::spawn(async {
        // Honour ADOS_RUN_DIR so a redirected runtime layout (a non-root dev host
        // or a test) places the socket alongside the other run-dir sockets.
        let sock = ados_groundlink::paths::run_path("groundlink-cmd.sock");
        if let Err(e) = ados_groundlink::cmdsock::serve(std::path::Path::new(&sock)).await {
            tracing::warn!(error = %e, path = %sock, "groundlink command socket exited");
        }
    });

    let role = resolve_role();
    match role {
        Role::Relay => {
            tracing::info!("ground-station relay role starting");
            run_relay_or_receiver(true, &mut sigterm, &mut sigint).await;
        }
        Role::Receiver => {
            tracing::info!("ground-station receiver role starting");
            run_relay_or_receiver(false, &mut sigterm, &mut sigint).await;
        }
        Role::Direct => {
            run_direct(&mut sigterm, &mut sigint).await?;
        }
    }

    tracing::info!("ground-station data-plane stopping");
    Ok(())
}

/// Run the relay (`is_relay`) or receiver loop until a shutdown signal. The
/// chosen loop owns its own adapter detect + monitor-mode + mDNS + state file;
/// a SIGTERM/SIGINT fires the shared `Notify` so the loop tears down cleanly.
async fn run_relay_or_receiver(
    is_relay: bool,
    sigterm: &mut tokio::signal::unix::Signal,
    sigint: &mut tokio::signal::unix::Signal,
) {
    let shutdown = Arc::new(Notify::new());

    // Telemetry emitter for the relay/receiver branch: ships the mesh snapshot
    // and the relay/receiver state to the logging daemon as the durable read
    // source the REST layer reads back. Best-effort and non-blocking; the direct
    // path constructs its own inside the receive loop. A second instance in this
    // process is fine.
    let ingest = ados_protocol::logd::emitter::IngestEmitter::new("ados-groundlink");

    // Observability: publish the mesh snapshot (neighbors / gateways /
    // selected-gateway) so the REST layer + OLED see the fabric. This is the
    // same poll the direct path skips; the relay/receiver FEC supervision below
    // is independent of it.
    let role_label = if is_relay { "relay" } else { "receiver" };
    let snap = mesh::MeshSnapshot::new(role_label, "bat0", "802.11s");
    tokio::spawn(mesh::run_poll_loop(snap, Some(ingest.clone())));

    // Atlas world-model aux-lane relay (off the WFB aux stream onto the LAN). Inert
    // unless this node is the relay role AND `ground_station.atlas.enabled` with a
    // configured compute base URL. It shares the role shutdown `Notify`, so a
    // SIGTERM/SIGINT tears it down with the rest of the relay. A non-Atlas ground
    // station spawns nothing here and is byte-unchanged.
    let atlas_task = maybe_spawn_atlas_relay(is_relay, shutdown.clone(), Some(ingest.clone()));

    let role_task = {
        let shutdown = shutdown.clone();
        let ingest = Some(ingest.clone());
        tokio::spawn(async move {
            if is_relay {
                relay::run(shutdown, ingest).await;
            } else {
                receiver::run(shutdown, ingest).await;
            }
        })
    };
    tokio::select! {
        _ = role_task => {}
        _ = sigterm.recv() => {
            tracing::info!("received SIGTERM");
            shutdown.notify_waiters();
        }
        _ = sigint.recv() => {
            tracing::info!("received SIGINT");
            shutdown.notify_waiters();
        }
    }
    // Give the loop a moment to flush its down-state on signal-triggered exit. The
    // Atlas relay self-stops on the shared `Notify`; the abort is a no-op if it
    // already returned, and reaps it on the role-task-exit path (no signal fired).
    tokio::time::sleep(Duration::from_millis(200)).await;
    if let Some(t) = atlas_task {
        t.abort();
    }
}

/// Spawn the ground-station Atlas aux-lane relay when (and only when) this node is
/// in the `relay` role and `ground_station.atlas.enabled` is set with a configured
/// `compute_base_url`. Returns the task handle so the caller can reap it on
/// teardown, or `None` when Atlas is disabled / not the relay role. When
/// `compute_base_url` is unset the task auto-resolves the workstation node over
/// mDNS (retrying until it answers or shutdown). Inert by default → a non-Atlas
/// ground station never reads the block and is byte-unchanged.
///
/// The relay reads the decoded WFB aux datagrams (the `wfb_rx -p 2` re-emit
/// loopback port, `ground_station.atlas.listen_port`, defaulting to the first
/// drone slot's `AUX_RX_PORT_BASE + slot`) and re-POSTs each framed Atlas event
/// onto the LAN into the compute node's event router, so the field RF lane
/// reaches the same receiver the direct-LAN bearer uses.
fn maybe_spawn_atlas_relay(
    is_relay: bool,
    shutdown: Arc<Notify>,
    ingest: Option<ados_protocol::logd::emitter::IngestEmitter>,
) -> Option<tokio::task::JoinHandle<()>> {
    if !is_relay {
        return None;
    }
    let cfg =
        ados_groundlink::GroundStationConfig::load_from(std::path::Path::new(CONFIG_YAML)).atlas;
    if !cfg.enabled {
        return None;
    }
    let listen_port = cfg.listen_port;
    let configured_url = cfg.compute_base_url.filter(|u| !u.trim().is_empty());
    Some(tokio::spawn(async move {
        // Use the configured compute base URL, or auto-resolve the workstation
        // node over mDNS so a field relay needs no hand-configured URL. The
        // resolve loop self-stops on the shared shutdown Notify.
        let compute_url = match configured_url {
            Some(url) => url,
            None => {
                tracing::info!(
                    "ground_station.atlas.compute_base_url unset; auto-resolving the compute node over mDNS"
                );
                loop {
                    if let Some(url) =
                        ados_groundlink::mdns::resolve_compute_base_url(Duration::from_secs(5))
                            .await
                    {
                        tracing::info!(compute_url = %url, "auto-resolved the compute node over mDNS");
                        break url;
                    }
                    tokio::select! {
                        _ = shutdown.notified() => return,
                        _ = tokio::time::sleep(Duration::from_secs(10)) => {}
                    }
                }
            }
        };
        tracing::info!(
            listen_port,
            compute_url = %compute_url,
            "starting ground-station Atlas aux-lane relay"
        );
        match ados_groundlink::run_atlas_relay(listen_port, compute_url, shutdown, ingest).await {
            Ok(stats) => tracing::info!(?stats, "atlas relay exited"),
            Err(e) => tracing::warn!(error = %e, "atlas relay failed to bind/run"),
        }
    }))
}

/// The standalone (`direct`) receive plane.
async fn run_direct(
    sigterm: &mut tokio::signal::unix::Signal,
    sigint: &mut tokio::signal::unix::Signal,
) -> Result<()> {
    let config = match wait_for_fleet_identity(sigterm, sigint).await {
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
    // Shared resolved-iface cell, written by the receive loop once it auto-detects
    // the injection adapter. Created here (before the listener spawn) so the hop
    // follower can read the live receive interface to retune it on a drone hop;
    // the shutdown path below also restores that adapter to managed mode.
    let resolved_iface: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    // The hop follower retunes the GS receive radio to a drone-announced channel
    // at the announce epoch, so a coordinated hop is a brief dwell-synced retune
    // rather than a blackout the valid-packet watchdog has to sweep out of.
    let hop_follower =
        presence::HopFollower::new(Arc::new(IwChannelSetter), resolved_iface.clone());
    tokio::spawn(presence::listen_supervisor(
        presence_cache.clone(),
        Some(hop_follower),
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
    // Deliberately confined to this role. The Atlas relay reads the SAME decoded
    // aux port in the relay role, and only one process may hold a UDP bind, so
    // running both would leave the loser dead. They do not meet today because
    // the Atlas relay is spawned only for the relay role and this only for
    // direct. Anything that later wants both on one node has to demultiplex the
    // lane once and fan out in-process, not bind the port twice.
    let aux_counters = ados_groundlink::AuxCounters::new();
    let aux_shutdown = Arc::new(Notify::new());
    // The relayed-node cache: status and identity frames the linked drone pushes
    // over the lane, held per device id with the age of each. Its sidecar is what
    // lets this node describe what it relays to an operator who is paired only
    // here, and it feeds the peer identity into the linked-peers surface.
    let aux_peers = ados_groundlink::aux_peers::AuxPeerCache::new();
    // One consumer per registered slot: the receive plane decodes each drone's
    // aux lane to its own loopback egress, so a single bind would hear exactly
    // one drone and every other drone's MAVLink, relayed status and RPC
    // responses would land on a port nobody reads. They share the counters, the
    // peer cache, the MAVLink ingest and the RPC response ingest, all of which
    // are already keyed per device id, so the fleet reports as one aggregate.
    let mavlink_ingest = Arc::new(ados_protocol::mavlink_ingest::MavlinkIngest::at_default_path());
    let spawn_aux_consumer = {
        let mavlink_ingest = mavlink_ingest.clone();
        let aux_counters = aux_counters.clone();
        let aux_peers = aux_peers.clone();
        let aux_shutdown = aux_shutdown.clone();
        move |slot: u8| -> tokio::task::JoinHandle<()> {
            tokio::spawn(ados_groundlink::supervise_aux_consumer(
                slot,
                wfb_rx::aux_rx_port(slot),
                mavlink_ingest.clone(),
                aux_counters.clone(),
                aux_peers.clone(),
                Some(ados_protocol::aux_rpc_proxy::AuxRpcResponseIngest::new(
                    ados_protocol::aux_rpc_proxy::DEFAULT_RESPONSE_SOCK,
                )),
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
    let aux_peers_task = tokio::spawn(ados_groundlink::aux_peers::persist_loop(
        aux_peers.clone(),
        Some(presence_cache.clone()),
        aux_shutdown.clone(),
    ));

    // The receive adapter is auto-detected inside the receive loop (config's
    // interface is often empty). The shared `resolved_iface` cell (created above
    // for the hop follower) is the seam the loop writes once it resolves the
    // injection adapter; on a shutdown signal this side restores that adapter to
    // managed mode, the mirror of the drone-side teardown, so the
    // kernel/NetworkManager can re-enumerate the RTL instead of finding it
    // stranded in monitor mode after the unit stops.

    // Run the receive loop until a shutdown signal arrives.
    tokio::select! {
        _ = receive_loop(&config, presence_cache, resolved_iface.clone(), aux_counters) => {}
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
    aux_shutdown.notify_waiters();
    tokio::time::sleep(Duration::from_millis(100)).await;
    // Stop the reconciler before reaping, so it cannot spawn a fresh consumer
    // into the map while the shutdown path is draining it.
    aux_reconcile.abort();
    for t in aux_tasks.lock().await.values() {
        t.abort();
    }
    // The persister waits on the same signal, but `notify_waiters` only wakes a
    // task already parked on it, so the abort is what reliably reaps it if the
    // signal landed while it was mid-write.
    aux_peers_task.abort();

    // Restore the resolved injection adapter to managed mode on the way out.
    restore_managed_if_resolved(&resolved_iface).await;
    Ok(())
}

/// The registered slots that have no auxiliary consumer yet.
///
/// Additive by design: a slot that is bound but no longer registered is
/// deliberately left alone rather than torn down. Holding the UDP bind for the
/// whole service lifetime is what keeps a consumer from losing its port to its
/// own lingering socket, and a slot that is later reissued finds its reader
/// already warm.
fn aux_slots_to_bind(want: &[u8], bound: &std::collections::BTreeSet<u8>) -> Vec<u8> {
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
async fn wait_for_fleet_identity(
    sigterm: &mut tokio::signal::unix::Signal,
    sigint: &mut tokio::signal::unix::Signal,
) -> Option<WfbConfig> {
    let mut logged = false;
    loop {
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

/// Restore the receive-plane adapter to managed mode on shutdown when one was
/// resolved this run. A no-op when the loop never selected an adapter (nothing
/// to restore). The read decision is split into [`iface_to_restore`] so the
/// capture-then-restore path is unit-testable without a real NIC.
async fn restore_managed_if_resolved(resolved: &Arc<Mutex<Option<String>>>) {
    if let Some(iface) = iface_to_restore(resolved).await {
        tracing::info!(interface = %iface, "restoring receive adapter to managed mode");
        ados_radio::adapter::set_managed_mode(&iface).await;
    }
}

/// Read the shared "last resolved iface" cell and return the interface to
/// restore (`Some`) or nothing to do (`None`). Pure over the cell, so the
/// capture (write from the receive loop) and the read (shutdown side) can be
/// asserted in a unit test without touching a NIC.
async fn iface_to_restore(resolved: &Arc<Mutex<Option<String>>>) -> Option<String> {
    resolved.lock().await.clone()
}

/// The receive manager's main loop: spawn a generation, run it to completion,
/// restart with bounded backoff. Mirrors the Python `WfbRxManager.run` structure
/// (sans the Python-owned adapter-detect/pairing gate).
async fn receive_loop(
    config: &WfbConfig,
    presence_cache: GsPresenceCache,
    resolved_iface: Arc<Mutex<Option<String>>>,
    aux_counters: ados_groundlink::AuxCounters,
) {
    let mut manager = WfbRxManager::new(config.clone());
    let clock: Arc<dyn ados_groundlink::watchdog::Clock> = Arc::new(SystemClock::default());
    let setter: Arc<dyn ados_groundlink::acquire::ChannelSetter> = Arc::new(IwChannelSetter);
    let hint = wfb_rx::default_hint();
    // Telemetry emitter for the per-generation receive-link samples shipped to
    // the logging daemon. Constructed once for the service lifetime; each
    // generation spawns a 1 Hz task that clones it. Best-effort and
    // non-blocking, like the drone-side radio emitter.
    let ingest = ados_protocol::logd::emitter::IngestEmitter::new("ados-groundlink");

    let mut backoff = 1.0_f64;
    // When the unpaired sidecar was last refreshed. The gate polls every 5 s so a
    // key landing is picked up promptly, but the sidecar only needs to stay
    // fresh, not be rewritten twelve times a minute on a flash card.
    let mut unpaired_published: Option<std::time::Instant> = None;
    loop {
        // Pairing gate: without the rx key on disk there is nothing to receive.
        // (The Python side blocks here too; the pairing flow lands the key.)
        if !std::path::Path::new(RX_KEY).exists() {
            tracing::info!(expected = RX_KEY, "ground_wfb_blocked_unpaired");
            // Say so on the sidecar too. This arm used to `continue` without
            // writing anything, so a ground station with no key published NOTHING
            // — and an unlinked pair had one half simply missing from every
            // surface but the journal.
            let due = unpaired_published
                .map(|t| t.elapsed() >= UNPAIRED_SIDECAR_REFRESH)
                .unwrap_or(true);
            if due {
                wfb_rx::write_blocked_unpaired_sidecar(
                    &config.interface,
                    config.rendezvous_channel(),
                    config,
                    Some(&ingest),
                );
                unpaired_published = Some(std::time::Instant::now());
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }
        // Paired again: the next unpaired spell publishes immediately rather than
        // waiting out a refresh interval left over from the last one.
        unpaired_published = None;
        // Resolve the receive adapter. Honors an explicit `video.wfb.interface`
        // override; otherwise auto-detects the RTL injection adapter (the
        // management wifi and the operator's control path are excluded and
        // monitor mode is proven) — symmetric with the drone-side selection, so
        // the ground station resolves its own adapter instead of idling until an
        // external detector supplies one.
        let (interface, adapter) = match ados_radio::adapter::select_interface(&config.interface)
            .await
        {
            Some(sel) if sel.injection_ok => {
                // Carry the full adapter record (chipset, injection verdict,
                // USB link health) onto the manager, so every sidecar write
                // reports the adapter actually in use rather than a default.
                let adapter = wfb_rx::GsAdapterInfo::from(&sel);
                if adapter.usb_degraded {
                    tracing::warn!(
                        interface = %sel.ifname,
                        usb_speed_mbps = ?adapter.usb_speed_mbps,
                        "ground_wfb_adapter_usb_degraded: adapter on a slow USB link (needs 480 Mbps); RF may not be received"
                    );
                }
                manager.set_adapter(adapter.clone());
                manager.set_interface(sel.ifname.clone());
                // Record the resolved injection adapter so the shutdown path
                // (in `run_direct`) can restore it to managed mode.
                *resolved_iface.lock().await = Some(sel.ifname.clone());
                (sel.ifname, adapter)
            }
            Some(sel) => {
                // Injection setup did not establish. A slow USB link is the usual
                // cause, so keep the USB facts and publish a `no_injection`
                // sidecar carrying them: a stuck receive plane must self-report
                // WHY it is deaf (Rule 44) rather than going silent while the run
                // loop retries. Without this write a slow-USB adapter — exactly
                // what lands a rig in this arm — reports nothing at all.
                let adapter = wfb_rx::GsAdapterInfo::from(&sel);
                manager.set_adapter(adapter.clone());
                wfb_rx::write_no_injection_sidecar(
                    &sel.ifname,
                    &adapter,
                    config.rendezvous_channel(),
                    config,
                    Some(&ingest),
                );
                tracing::warn!(
                    interface = %sel.ifname,
                    usb_speed_mbps = ?adapter.usb_speed_mbps,
                    usb_degraded = adapter.usb_degraded,
                    "ground_wfb_adapter_no_injection"
                );
                tokio::time::sleep(Duration::from_secs(backoff as u64)).await;
                backoff = (backoff * 2.0).min(30.0);
                continue;
            }
            None => {
                manager.set_adapter(wfb_rx::GsAdapterInfo::default());
                tracing::warn!("ground_no_wfb_adapter_found");
                tokio::time::sleep(Duration::from_secs(backoff as u64)).await;
                backoff = (backoff * 2.0).min(30.0);
                continue;
            }
        };

        // Bring the interface to receive-ready BEFORE the spawn, in the
        // kernel-required order: the regulatory gate (set + verify the domain,
        // then assert the rendezvous channel is permitted and non-DFS, both
        // before monitor-mode bring-up so the home channel is never capped to the
        // startup domain's limits), then monitor mode, TX power (brownout guard
        // on marginal USB hosts), and the rendezvous-home channel. Re-applied
        // each generation. On a strict-gate failure the receive chain is NOT
        // spawned on a capped radio: park in `reg_blocked`, surface it, and
        // retry with bounded backoff.
        if let Err(e) = manager.prepare_interface(&interface).await {
            // Surface the live domain vs the wanted one (the manager's snapshot
            // may be partial when the gate failed before reading it), so the panel
            // shows the actual regulatory conflict, not a configured-channel lie.
            let wanted = config
                .reg_domain
                .clone()
                .filter(|d| !d.is_empty())
                .unwrap_or_else(|| wfb_rx::DEFAULT_REG_DOMAIN.to_string());
            let status = ados_radio::adapter::read_reg_status(&wanted).await;
            let reg = wfb_rx::GsRegSnapshot {
                domain: status.domain,
                verified: status.verified,
                enabled_channels: manager.enabled_channels().iter().copied().collect(),
            };
            wfb_rx::write_reg_blocked_sidecar(
                &interface,
                &adapter,
                config.rendezvous_channel(),
                config,
                &reg,
                e.reason_code(),
                Some(&ingest),
            );
            tokio::time::sleep(Duration::from_secs(backoff as u64)).await;
            backoff = (backoff * 2.0).min(30.0);
            continue;
        }

        // Resolve this generation's fleet slots and spawn the receive chain.
        // The lowest registered slot is the PRIMARY: its video RX carries the
        // stats stream, anchors the generation's liveness, and is the link the
        // channel acquirer sweeps for. Every other registered slot gets its own
        // additive video/aux/control trio on the same interface.
        let mut slots = wfb_rx::fleet_slots(&FleetRegistry::load(std::path::Path::new(
            FLEET_REGISTRY_PATH,
        )));
        slots.sort_unstable();
        let primary_slot = slots[0];
        let mut chain = match manager.spawn_receive_chain(&interface, primary_slot).await {
            Ok(chain) => chain,
            Err(e) => {
                tracing::error!(error = %e, slot = primary_slot, "ground_wfb_rx_failed_to_start");
                tokio::time::sleep(Duration::from_secs(backoff as u64)).await;
                backoff = (backoff * 2.0).min(5.0);
                continue;
            }
        };
        // Secondary slots are best-effort: a failure to spawn one drone's
        // receivers must not take the whole fleet's receive plane down, and the
        // reconcile tick below retries it on the next pass.
        let mut secondaries: std::collections::BTreeMap<u8, SlotReceivers> =
            std::collections::BTreeMap::new();
        for &slot in &slots[1..] {
            match manager.spawn_slot_receivers(&interface, slot, false).await {
                Ok(r) => {
                    secondaries.insert(slot, r);
                }
                Err(e) => tracing::error!(error = %e, slot, "ground_slot_rx_failed_to_start"),
            }
        }
        tracing::info!(
            fleet_id = manager.fleet_id(),
            primary_slot,
            slots = ?slots,
            "ground_receive_chain_spawned"
        );
        backoff = 1.0;

        let stdout = chain.video.take_stdout();
        // The rest of the chain (the primary's aux + control receivers and the
        // two ground transmitters) is held for the generation's lifetime; its
        // `Drop` killpg's each process group when this iteration ends.
        let _primary_rest = (chain.aux, chain.control, chain.tx_control, chain.aux_tx);
        let rx_handle = DataRxHandle::new(chain.video);

        // Shared liveness state for this generation.
        let counter = SharedValidCounter::new();
        let link = Arc::new(Mutex::new(LinkStats::default()));
        let last_stdout_at = Arc::new(Mutex::new(clock.monotonic()));
        let zombie_kills = Arc::new(AtomicU32::new(0));
        // Live receive-health publish seam: the valid-packet watchdog writes its
        // reacquire-kill total + the valid-decode silence here, and the stats
        // reader pulls them onto the sidecar so the GS heartbeat carries the real
        // values instead of hardcoded zeros.
        let rx_health = wfb_rx::SharedRxHealth::new();

        // Fan-out as a sub-service (the HERO slot's video egress → 5600 mediamtx
        // + 5605 LCD), aborted with the generation. It follows the operator's
        // hero selection, published as a sidecar by the hero route, and re-points
        // itself when that changes; the primary slot is what it serves when no
        // live selection exists, which is the boot state and the permanent state
        // of a single-drone fleet. The shared counters are read by the stats
        // reader so the wfb-stats sidecar surfaces the forwarded/drop totals (the
        // fan-out hop, otherwise blind to the cross-process diagnostics).
        let fanout_counters = fanout::FanoutCounters::new();
        let fanout_task = tokio::spawn(fanout::run_default_fanout(
            primary_slot,
            fanout_counters.clone(),
        ));

        // 1 Hz receive-link telemetry for this generation: ship the link's
        // RSSI / SNR / uncorrected-FEC (the uplink command radio, mirroring the
        // drone-side downlink video radio) and a lock/unlock event on a real
        // link-state transition. Aborted with the generation. Best-effort; an
        // absent logging daemon drops the samples without disturbing receive.
        let telemetry_task = {
            let emitter = ingest.clone();
            let link = link.clone();
            tokio::spawn(async move {
                use ados_protocol::logd::{Fields, Level, Value};
                let mut tick = tokio::time::interval(Duration::from_secs(1));
                let mut prev_locked: Option<bool> = None;
                loop {
                    tick.tick().await;
                    let stats = link.lock().await.clone();
                    let rx_key_present = std::path::Path::new(RX_KEY).exists();
                    // A ground station is a receive end: it never injects the
                    // video plane, so `tx_live` is false and the unverified
                    // branch is structurally unreachable here. It proves the
                    // link by its own decodes, which the stats above carry.
                    let state = ados_radio::link_state::derive_link_state(
                        rx_key_present,
                        false,
                        &stats,
                        false,
                        false,
                    );
                    let mut tags = Fields::new();
                    tags.insert("direction".to_string(), Value::from("uplink"));
                    tags.insert("link".to_string(), Value::from("command"));
                    emitter.emit_metric("link.rssi_dbm", stats.rssi_dbm, tags.clone());
                    emitter.emit_metric("link.snr_db", stats.snr_db, tags.clone());
                    emitter.emit_metric("link.fec_uncorrected", stats.fec_failed as f64, tags);
                    let locked = state.is_locked();
                    if prev_locked != Some(locked) {
                        let mut detail = Fields::new();
                        detail.insert("link".to_string(), Value::from("command"));
                        detail.insert("state".to_string(), Value::from(state.as_str()));
                        if locked {
                            emitter.emit_event("link.lock", Level::Info, detail);
                        } else if prev_locked.is_some() {
                            emitter.emit_event("link.unlock", Level::Warn, detail);
                        }
                        prev_locked = Some(locked);
                    }
                }
            })
        };

        // 1 Hz link-quality feedback to the transmitting drone. The drone cannot
        // measure its own downlink, so its adaptive bitrate ladder has no loss
        // sample to step on; this receiver has one and reports it up the aux
        // uplink. Aborted with the generation, exactly like the telemetry task,
        // so a receive chain that is being torn down stops asserting a link
        // quality it is no longer measuring.
        let feedback_task = {
            let link = link.clone();
            let port = config.aux_tx_port;
            tokio::spawn(ados_groundlink::link_feedback::run(
                link,
                port,
                primary_slot,
            ))
        };

        // Stats reader: feeds the counter + LinkStats + the sidecar. Carries the
        // rendezvous home, the regulatory snapshot the gate resolved, and the
        // resolved adapter facts so the sidecar surfaces the truthful channel,
        // reg picture, and adapter/USB health, symmetric with the drone side.
        let stats_task = stdout.map(|out| {
            tokio::spawn(wfb_rx::stats_reader_loop(
                out,
                counter.clone(),
                link.clone(),
                last_stdout_at.clone(),
                clock.clone(),
                interface.clone(),
                manager.channel(),
                manager.rendezvous_channel(),
                manager.reg_snapshot().clone(),
                config.clone(),
                manager.adapter().clone(),
                Some(rx_health.clone()),
                zombie_kills.clone(),
                Some(ingest.clone()),
                fanout_counters.clone(),
                aux_counters.clone(),
            ))
        });

        // Zombie watchdog (stdout-silence).
        let mut zombie_task = tokio::spawn(wfb_rx::zombie_watchdog(
            rx_handle.clone(),
            last_stdout_at.clone(),
            clock.clone(),
            zombie_kills.clone(),
        ));

        // Valid-packet watchdog: owns a fresh acquirer, reads the shared counter
        // + presence cache, terminates the data RX on a genuine loss. It also
        // observes live video off the shared counter each poll (so a healthy
        // stream with a dropped peer beacon does not trip the teardown) and
        // mirrors its receive-health counters to the stats reader's sidecar.
        let mut watchdog = manager
            .build_watchdog(
                counter.clone(),
                presence_cache.clone(),
                rx_handle.clone(),
                clock.clone(),
                setter.clone(),
                hint.clone(),
            )
            .with_health(rx_health.clone());
        let mut watchdog_task = tokio::spawn(async move {
            watchdog.run().await;
        });

        // Re-read the fleet registry on a slow tick and add/remove the SECONDARY
        // slots' receivers in place. Pairing a 25th drone must not interrupt the
        // other 24, so a registry change reconciles inside the generation rather
        // than restarting the whole chain. Dropping a `SlotReceivers` killpg's
        // its three process groups, which is the despawn.
        //
        // Returning from this arm ENDS the generation, and it only returns when
        // the PRIMARY slot is released: the primary's video RX is the stats
        // stream and the channel acquirer's target, so a new primary has to be
        // chosen by a fresh generation.
        let reconcile = async {
            let mut tick = tokio::time::interval(FLEET_RECONCILE_INTERVAL);
            tick.tick().await; // the first tick completes immediately
            let mut hero_tick = tokio::time::interval(Duration::from_secs(1));
            hero_tick.tick().await;
            let hero_path = std::path::PathBuf::from(ados_groundlink::fleet_hero::hero_path());
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        let want: std::collections::BTreeSet<u8> = wfb_rx::fleet_slots(
                            &FleetRegistry::load(std::path::Path::new(FLEET_REGISTRY_PATH)),
                        )
                        .into_iter()
                        .collect();
                        if !want.contains(&primary_slot) {
                            tracing::info!(primary_slot, "ground_primary_slot_released");
                            return;
                        }
                        secondaries.retain(|slot, _| {
                            let keep = want.contains(slot);
                            if !keep {
                                tracing::info!(slot, "ground_slot_rx_despawned");
                            }
                            keep
                        });
                        // Collect the missing slots before spawning: the filter
                        // borrows `secondaries` and the insert needs it mutably.
                        let missing: Vec<u8> = want
                            .iter()
                            .copied()
                            .filter(|s| *s != primary_slot && !secondaries.contains_key(s))
                            .collect();
                        for slot in missing {
                            match manager.spawn_slot_receivers(&interface, slot, false).await {
                                Ok(r) => {
                                    secondaries.insert(slot, r);
                                    tracing::info!(slot, "ground_slot_rx_spawned");
                                }
                                Err(e) => {
                                    tracing::error!(
                                        error = %e, slot, "ground_slot_rx_failed_to_start"
                                    )
                                }
                            }
                        }
                    }
                    // Video bring-back, driven off the 1 s hero poll: only the
                    // hero slot's video egress is read, so a secondary slot
                    // decodes a full-FEC video stream nobody consumes. Bring the
                    // video receiver up when the slot BECOMES the hero and drop
                    // it (killpg) when it is unpromoted. `aux` + `control` were
                    // spawned unconditionally and stay up either way.
                    _ = hero_tick.tick() => {
                        let hero = fanout::resolve_fanout_slot(
                            primary_slot,
                            &hero_path,
                            std::path::Path::new(FLEET_REGISTRY_PATH),
                        );
                        for (slot, rx) in secondaries.iter_mut() {
                            let is_hero = *slot == hero;
                            if is_hero && !rx.decoding_video() {
                                match manager.spawn_slot_video(&interface, *slot).await {
                                    Ok(v) => {
                                        rx.video = Some(v);
                                        tracing::info!(slot, "ground_slot_video_brought_up");
                                    }
                                    Err(e) => tracing::error!(
                                        error = %e, slot, "ground_slot_video_failed_to_start"
                                    ),
                                }
                            } else if !is_hero && rx.decoding_video() {
                                // Dropping the receiver killpg's its group.
                                rx.video = None;
                                tracing::info!(slot, "ground_slot_video_unwatched");
                            }
                        }
                    }
                }
            }
        };

        // The generation ends when any of: the data RX exits, the zombie
        // watchdog kills it, the valid-packet watchdog terminates it, or the
        // primary slot leaves the fleet.
        // `&mut` the watchdog handles so the arm that did NOT win is not
        // dropped-and-detached here — a dropped JoinHandle leaves the task
        // running, so the zombie + valid-packet watchdogs would pile up across
        // generations, each holding an acquirer + driving `iw` retunes against
        // the next generation's radio. They are aborted explicitly below.
        tokio::select! {
            _ = wait_for_exit(rx_handle.clone()) => {
                tracing::warn!("ground_wfb_rx_exited");
            }
            _ = &mut zombie_task => {}
            _ = &mut watchdog_task => {}
            _ = reconcile => {}
        }

        // Tear down the generation's sub-tasks before respawning. The two
        // watchdog handles are aborted alongside the fan-out / telemetry / stats
        // tasks (an already-finished task's abort is a no-op), so no generation's
        // watchdog survives into the next, mirroring the air-side abort-siblings
        // discipline.
        zombie_task.abort();
        watchdog_task.abort();
        fanout_task.abort();
        telemetry_task.abort();
        feedback_task.abort();
        if let Some(t) = stats_task {
            t.abort();
        }

        tokio::time::sleep(Duration::from_secs(backoff as u64)).await;
        backoff = (backoff * 2.0).min(5.0);
    }
}

/// Poll the data-RX handle until it reports not-running. One arm of the
/// generation's completion select.
async fn wait_for_exit(rx: Arc<DataRxHandle>) {
    use ados_groundlink::watchdog::RxProcess;
    loop {
        if !rx.is_running() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_slot_registered_after_start_still_gets_a_consumer() {
        use std::collections::BTreeSet;

        // The service started with one drone paired.
        let bound: BTreeSet<u8> = [1u8].into_iter().collect();

        // A second drone pairs. Its slot is issued and the receive plane spawns
        // a wfb_rx for it, but the pair route does not restart this unit when
        // the fleet key is unchanged, so nothing else would notice. Before the
        // reconcile, slot 2 decoded onto a port with no reader.
        assert_eq!(
            aux_slots_to_bind(&[1, 2], &bound),
            vec![2],
            "a slot issued after start must be picked up"
        );

        // A whole fleet joining at once is bound in one pass.
        assert_eq!(aux_slots_to_bind(&[1, 2, 3, 4], &bound), vec![2, 3, 4]);

        // Steady state does nothing: no rebinding of a live port.
        let all: BTreeSet<u8> = [1u8, 2, 3, 4].into_iter().collect();
        assert!(aux_slots_to_bind(&[1, 2, 3, 4], &all).is_empty());

        // A released slot is NOT torn down, and does not come back as work.
        assert!(
            aux_slots_to_bind(&[1], &all).is_empty(),
            "a deregistered slot must not be rebound or reaped here"
        );
    }

    #[test]
    fn explicit_role_relay_wins() {
        let role = resolve_role_from(&args(&["--role", "relay"]), Some("direct"));
        assert_eq!(role, Role::Relay);
    }

    #[test]
    fn explicit_role_eq_form_receiver() {
        let role = resolve_role_from(&args(&["--role=receiver"]), Some("direct"));
        assert_eq!(role, Role::Receiver);
    }

    #[test]
    fn sentinel_decides_with_no_argument() {
        let role = resolve_role_from(&[], Some("relay"));
        assert_eq!(role, Role::Relay);
    }

    #[test]
    fn unknown_explicit_value_falls_through_to_direct() {
        let role = resolve_role_from(&args(&["--role", "bogus"]), None);
        assert_eq!(role, Role::Direct);
    }

    #[test]
    fn unknown_explicit_value_falls_through_to_sentinel() {
        // An unknown explicit arg is warned but does not strand the resolution:
        // it falls through to the sentinel, which here selects receiver.
        let role = resolve_role_from(&args(&["--role", "bogus"]), Some("receiver"));
        assert_eq!(role, Role::Receiver);
    }

    #[test]
    fn no_argument_and_no_sentinel_is_direct() {
        assert_eq!(resolve_role_from(&[], None), Role::Direct);
    }

    #[test]
    fn unknown_sentinel_is_direct() {
        assert_eq!(resolve_role_from(&[], Some("bogus")), Role::Direct);
    }

    #[tokio::test]
    async fn resolved_cell_holds_iface_after_capture() {
        // Mirror what the receive loop does after it resolves the injection
        // adapter: write the iface into the shared cell. The shutdown side reads
        // it back via the same helper it uses to decide whether to restore.
        let cell: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        // Nothing resolved yet → nothing to restore.
        assert_eq!(iface_to_restore(&cell).await, None);

        // Receive-loop capture point.
        *cell.lock().await = Some("wlan1".to_string());

        // Shutdown side reads the captured iface and would restore exactly it.
        assert_eq!(iface_to_restore(&cell).await, Some("wlan1".to_string()));
    }

    #[tokio::test]
    async fn restore_is_noop_when_no_iface_resolved() {
        // With an empty cell the restore decision yields None, so the shutdown
        // path performs no managed-mode restore (the no-adapter run).
        let cell: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        assert!(iface_to_restore(&cell).await.is_none());
    }
}
