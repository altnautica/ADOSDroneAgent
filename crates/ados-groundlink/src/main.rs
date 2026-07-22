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
    self, DataRxHandle, IwChannelSetter, SharedValidCounter, SystemClock, WfbRxManager,
};
use ados_groundlink::{fanout, mesh, presence, receiver, relay, GsPresenceCache};

const CONFIG_YAML: &str = "/etc/ados/config.yaml";
const RX_KEY: &str = ados_radio::paths::WFB_RX_KEY;

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
/// The relay reads the decoded WFB aux datagrams (the `wfb_rx` re-emit loopback
/// port, `ground_station.atlas.listen_port`, default 5603) and re-POSTs each
/// framed Atlas event onto the LAN into the compute node's event router, so the
/// field RF lane reaches the same receiver the direct-LAN bearer uses.
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
    let config = WfbConfig::load_from(std::path::Path::new(CONFIG_YAML));
    tracing::info!(
        channel = config.channel,
        band = %config.band,
        interface = %config.interface,
        "ground-station data-plane starting (direct role)"
    );

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

    // The receive adapter is auto-detected inside the receive loop (config's
    // interface is often empty). The shared `resolved_iface` cell (created above
    // for the hop follower) is the seam the loop writes once it resolves the
    // injection adapter; on a shutdown signal this side restores that adapter to
    // managed mode, the mirror of the drone-side teardown, so the
    // kernel/NetworkManager can re-enumerate the RTL instead of finding it
    // stranded in monitor mode after the unit stops.

    // Run the receive loop until a shutdown signal arrives.
    tokio::select! {
        _ = receive_loop(&config, presence_cache, resolved_iface.clone()) => {}
        _ = sigterm.recv() => {
            tracing::info!("received SIGTERM");
        }
        _ = sigint.recv() => {
            tracing::info!("received SIGINT");
        }
    }

    // Restore the resolved injection adapter to managed mode on the way out.
    restore_managed_if_resolved(&resolved_iface).await;
    Ok(())
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
    loop {
        // Pairing gate: without the rx key on disk there is nothing to receive.
        // (The Python side blocks here too; the pairing flow lands the key.)
        if !std::path::Path::new(RX_KEY).exists() {
            tracing::info!(expected = RX_KEY, "ground_wfb_blocked_unpaired");
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }
        // Resolve the receive adapter. Honors an explicit `video.wfb.interface`
        // override; otherwise auto-detects the RTL injection adapter (the
        // management wifi and the operator's control path are excluded and
        // monitor mode is proven) — symmetric with the drone-side selection, so
        // the ground station resolves its own adapter instead of idling until an
        // external detector supplies one.
        let (interface, chipset) =
            match ados_radio::adapter::select_interface(&config.interface).await {
                Some(sel) if sel.injection_ok => {
                    manager.set_adapter(Some(sel.chipset.clone()), true);
                    manager.set_interface(sel.ifname.clone());
                    // Record the resolved injection adapter so the shutdown path
                    // (in `run_direct`) can restore it to managed mode.
                    *resolved_iface.lock().await = Some(sel.ifname.clone());
                    (sel.ifname, Some(sel.chipset))
                }
                Some(sel) => {
                    manager.set_adapter(Some(sel.chipset.clone()), false);
                    tracing::warn!(interface = %sel.ifname, "ground_wfb_adapter_no_injection");
                    tokio::time::sleep(Duration::from_secs(backoff as u64)).await;
                    backoff = (backoff * 2.0).min(30.0);
                    continue;
                }
                None => {
                    manager.set_adapter(None, false);
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
                chipset.as_deref(),
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

        // Spawn the receive chain for this generation.
        let (mut data_rx, _rx_control, _tx_control) =
            match manager.spawn_receive_chain(&interface).await {
                Ok(chain) => chain,
                Err(e) => {
                    tracing::error!(error = %e, "ground_wfb_rx_failed_to_start");
                    tokio::time::sleep(Duration::from_secs(backoff as u64)).await;
                    backoff = (backoff * 2.0).min(5.0);
                    continue;
                }
            };
        backoff = 1.0;

        let stdout = data_rx.take_stdout();
        let rx_handle = DataRxHandle::new(data_rx);

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

        // Fan-out as a sub-service (5599 → 5600 mediamtx + 5605 LCD), aborted
        // with the generation. The shared counters are read by the stats reader
        // so the wfb-stats sidecar surfaces the forwarded/drop totals (the
        // fan-out hop, otherwise blind to the cross-process diagnostics).
        let fanout_counters = fanout::FanoutCounters::new();
        let fanout_task = tokio::spawn(fanout::run_default_fanout(fanout_counters.clone()));

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

        // Stats reader: feeds the counter + LinkStats + the sidecar. Carries the
        // rendezvous home + the regulatory snapshot the gate resolved so the
        // sidecar surfaces the truthful channel + reg picture, symmetric with the
        // drone side.
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
                None,
                true,
                Some(rx_health.clone()),
                zombie_kills.clone(),
                Some(ingest.clone()),
                fanout_counters.clone(),
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

        // The generation ends when any of: the data RX exits, the zombie
        // watchdog kills it, or the valid-packet watchdog terminates it.
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
