//! The parameterized daemon lifecycle: bind, serve, background tasks, shutdown.

use super::*;

/// The lifecycle, parameterized over the paths and the stop trigger so tests can
/// drive a real bring-up + shutdown against temp sockets without sending a
/// process signal.
///
/// Both listeners are bound up front so a bind clash (the FastAPI surface
/// already on the port, a stale socket) surfaces here rather than inside a
/// spawned task — important for the inert dual-run, where a port collision is
/// the first thing the cutover must rule out.
pub async fn run_with_paths<F>(paths: DaemonPaths, shutdown: F) -> Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    // One live security posture, shared by the native credential check, the
    // proxied-auth gate, the CORS allow-list and the routes, so they always read
    // the same snapshot of `security:` (re-read when the file changes, the last
    // good copy kept across a bad edit).
    let security = Arc::new(crate::security_state::SecurityState::new(
        paths.config_path.clone(),
    ));
    // Publish the startup parse result so a malformed config surfaces on the
    // fleet Health view, not just in the log (per-service status sidecar).
    let startup_error =
        crate::config::config_snapshot(&paths.config_path, crate::config::CONFIG_CACHE_TTL)
            .security
            .err();
    ados_config::write_config_status("control", startup_error.as_deref());
    let pairing = Arc::new(
        PairingState::with_path(paths.pairing_path.clone()).with_security(Arc::clone(&security)),
    );

    // The vehicle-state reader the status + telemetry routes project. Its
    // background task connects to the state socket and reconnects on EOF / an
    // absent socket; an idle agent (no socket) leaves the snapshot empty, which
    // the routes degrade to rather than fail. The handle stops it on shutdown.
    let (state_client, state_handle) = StateIpcClient::spawn(paths.state_socket.clone());
    // The battery health engine and the task that feeds it from the state
    // snapshot at 2 Hz, hot-reloading the `battery:` config block and recording
    // each anomaly transition in the logging store. Stopped with the readers.
    let (battery, battery_handle) =
        crate::battery::spawn(state_client.clone(), paths.config_path.clone());
    // The swarm neighbour-table reader. Same posture as the state reader: it
    // reconnects on EOF / an absent socket, and a node not running the bus leaves
    // nothing published, which the routes degrade to. Spawned on EVERY profile — a
    // drone answering `/api/swarm/neighbors` with the ground station powered off is
    // the decentralization proof, so this is deliberately not profile-gated.
    let (swarm_client, swarm_handle) = SwarmIpcClient::spawn(paths.swarm_socket.clone());
    // The MAVLink command client the command route writes frames through. It
    // connects lazily on the first command and reuses the connection; an absent
    // socket (an idle agent) surfaces as a 503 at command time, not here.
    let mavlink_client = MavlinkIpcClient::new(paths.mavlink_socket.clone());
    // The logging-store query client the status route reads system health from.
    // Each call opens a short-lived connection; an unreachable store degrades the
    // health block to its zero default rather than failing the route.
    let logd_client = LogdQueryClient::new(paths.logd_query_socket.clone());
    // The on-disk paths the pairing routes read + write. The pairing-state file
    // is the same one the LAN-edge auth reader watches, so the gate and the
    // claim/unpair writers agree on one file.
    let pairing_paths = PairingPaths {
        config: paths.config_path.clone(),
        pairing_json: paths.pairing_path.clone(),
        wfb_key_dir: paths.wfb_key_dir.clone(),
        bind_state: paths.bind_state_path.clone(),
        profile_conf: paths.profile_conf_path.clone(),
        mesh_role: paths.mesh_role_path.clone(),
        relay_secret: paths.relay_secret_path.clone(),
    };
    // The dashboard-access PIN store, shared (one `Arc`) between the routes
    // (set/verify/clear/status) and the LAN-edge auth (which accepts a valid
    // dashboard session token as an alternative credential), so both read one
    // record.
    let dashboard_pin = Arc::new(crate::dashboard_pin::DashboardPin::with_path(
        paths.dashboard_pin_path.clone(),
    ));
    // The MCP-token store, shared (one `Arc`) between the `/api/mcp/*` routes and
    // the LAN-edge auth (which, behind the accept flag, verifies a scoped MCP
    // token as an alternative credential), so both read one record.
    let mcp_tokens = Arc::new(crate::mcp::McpTokenStore::with_path(
        paths.mcp_token_path.clone(),
    ));
    let state = AppState::new(
        Arc::clone(&pairing),
        state_client,
        mavlink_client,
        logd_client,
        paths.board_path.clone(),
        pairing_paths.clone(),
        Arc::clone(&dashboard_pin),
        Arc::clone(&mcp_tokens),
    )
    .with_params_path(paths.params_path.clone())
    .with_run_dir(paths.run_dir.clone())
    .with_bin_dir(paths.bin_dir.clone())
    .with_swarm(swarm_client)
    .with_battery(battery)
    .with_security(Arc::clone(&security));

    // Native-vs-residual gate for the PIC / gamepad / Bluetooth writes, resolved
    // once at startup (the profile is fixed for the process). Those are served
    // natively only on a ground station, the only profile that runs the daemons
    // binding their command sockets (ados-pic's pic.sock, ados-input's
    // hid-cmd.sock); on any other node they fall through to the residual.
    let is_ground_station = {
        let cfg = crate::config::PairingConfig::cached(&paths.config_path);
        crate::profile::current_profile_and_role_at(
            &cfg.agent.profile,
            &paths.profile_conf_path,
            &paths.mesh_role_path,
        )
        .0 == "ground-station"
    };
    let hid_native = is_ground_station;

    // The Wi-Fi client routes talk to the Wi-Fi command socket on every profile.
    // A ground station's ados-net uplink daemon owns it; every other node has no
    // uplink daemon, so this process serves the socket itself with the same
    // Wi-Fi-client manager (the owner of the station's advisory radio lock).
    if !is_ground_station {
        let sock = crate::routes::network_write::wifi_cmd_sock();
        let runner: Arc<dyn ados_net::cmd::CmdRunner> = Arc::new(ados_net::cmd::TokioCmdRunner);
        let wifi = Arc::new(tokio::sync::Mutex::new(
            ados_net::managers::WifiClientManager::new(runner),
        ));
        tokio::spawn(async move {
            if let Err(e) = ados_net::cmdsock::serve_wifi_client(wifi, &sock).await {
                tracing::error!(path = %sock.display(), error = %e, "wifi_client_socket_bind_failed");
            }
        });
    }

    // Flight-controller identity reconciler: reports every minute whether this
    // aircraft's SYSID_THISMAV matches its fleet slot, and writes it only when
    // the operator switched the write on and the vehicle is disarmed. A ground
    // station drives no flight controller of its own, so it does not run one.
    if !is_ground_station {
        crate::routes::fc_identity::spawn_reconciler(state.clone());
    }

    // Build the relay-proxy caller on ground-station-profile nodes only. The
    // proxy's AuxEgress talks to the radio's aux command socket; the response
    // listener binds the IPC socket that the ados-groundlink aux consumer
    // forwards Response frames to.
    let state = if is_ground_station {
        // The ground station's wfb_rx manager spawns `wfb_tx -p3` whose loopback
        // ingress is the aux transmit port. Connect directly to it rather than
        // going through the `radio-aux.sock` command socket, which does not
        // exist on the ground station (the radio lifecycle is managed by the
        // groundlink process, not the ados-radio command socket).
        //
        // Resolved from config rather than hardcoded: the port is
        // operator-settable, and a literal here silently stopped matching the
        // radio the moment anyone changed it.
        let aux_tx = ados_protocol::aux_ports::AuxPorts::load().tx;
        match ados_protocol::aux_egress::AuxEgress::connected_to_udp(aux_tx).await {
            Ok(aux_egress) => {
                let proxy = Arc::new(ados_protocol::aux_rpc_proxy::AuxRpcProxy::new(aux_egress));
                // The listener spawns its own task, so wrapping the call in
                // another `tokio::spawn` only moved the cancel handle into a
                // future that dropped it immediately — nothing could ever
                // signal a shutdown, and the returned join handle went the same
                // way. Hold both on the router handle instead, so the shutdown
                // path this process will eventually grow has something to hold.
                let reader_cancel = Arc::new(tokio::sync::Notify::new());
                let reader_task = proxy.spawn_response_listener(
                    ados_protocol::aux_rpc_proxy::DEFAULT_RESPONSE_SOCK,
                    Arc::clone(&reader_cancel),
                );
                let listener = crate::state::AuxResponseListener::new(reader_cancel, reader_task);
                // Fleet attention reconciler: auto-promotes a one-drone fleet to
                // the full video profile (every drone boots to thumbnail, so the
                // existing single-drone product would otherwise sit at 320x180),
                // re-issues any assignment a hero selection could not confirm,
                // and re-asserts any drone whose beaconed hero bit disagrees
                // with the selection (a hero that rebooted comes back as a
                // thumbnail). Idle — zero radio traffic — once the fleet agrees.
                tokio::spawn(crate::routes::gs_fleet_hero::run_hero_reconciler(
                    Arc::clone(&proxy),
                    state.swarm.clone(),
                ));
                // Fleet slot delivery: the ground station allocates a slot at
                // pair time and, until now, told only the caller. A drone
                // learned its own fleet address only from a hand-edit or a
                // cloud push. Reachable even when the drone holds the wrong
                // slot, because the aux uplink is addressed on the ground
                // station's link id rather than the drone's. Idle once the
                // fleet agrees.
                tokio::spawn(crate::routes::gs_fleet_slot::run_slot_reconciler(
                    Arc::clone(&proxy),
                ));
                // Fleet enrolment: a slot was only ever issued by the pair
                // route, but the unattended auto-bind that pairs a rig in the
                // field has no device id to allocate against and never called
                // it. Both reconcilers above therefore had an empty registry to
                // work from on every auto-bound fleet, and the per-pair relay
                // secret they key on was never minted. Registers each drone the
                // ground station can actually hear.
                tokio::spawn(crate::routes::gs_fleet_enroll::run_enroll_reconciler());
                state
                    .with_aux_rpc_proxy(proxy)
                    .with_aux_response_listener(listener)
            }
            Err(e) => {
                tracing::warn!(error = %e, "aux_egress_connect_failed_relay_proxy_unavailable");
                state
            }
        }
    } else {
        state
    };

    // Peer device-id back-fill: the radio's hop supervisor latches the peer id it
    // learns from a verified presence beacon into `peer-backfill.json`, and until
    // now nothing read it — so an auto-bound rig's `paired_drone.device_id` was
    // null on every status read, across reboots. Runs on BOTH profiles: a drone
    // learns its ground station's id the same way, and the canonical key is
    // profile-agnostic. Idle once the config agrees.
    tokio::spawn(crate::routes::peer_backfill::run_peer_backfill_reconciler(
        paths.config_path.clone(),
        is_ground_station,
    ));

    // The proxied-route auth decision: the ported Python auth + HMAC middlewares
    // the front runs on every forwarded (non-native) route, so the front is the
    // single authenticator for the whole surface. It reads the configured key +
    // HMAC settings from the live security posture per request. Built once and
    // shared across all connections (it holds the replay-detector nonce store).
    let proxied_auth = Arc::new(crate::proxy_auth::ProxiedAuth::live(Arc::clone(&security)));

    // The Unix edge: the bare Router, no auth. The LAN edge: the same Router
    // wrapped with the rate-limit + auth layer keyed on the shared pairing
    // reader (so a route and the gate read one short-TTL-cached posture).
    let unix_router = unix_app(build_router(state.clone(), hid_native));
    let tcp_router = tcp_app(
        build_router(state, hid_native),
        Arc::clone(&pairing),
        Arc::clone(&proxied_auth),
        Arc::clone(&dashboard_pin),
        Arc::clone(&mcp_tokens),
        paths.config_path.clone(),
        security,
    );

    // Bind every listener up front so a bind failure surfaces here. The LAN front
    // binds an AF_INET and (best-effort) an AF_INET6 socket on the same port, so a
    // browser that resolves a `*.local` host to both A and AAAA records reaches
    // the front whichever family it tries first.
    let unix_listener = bind_unix(&paths.control_socket)
        .with_context(|| format!("bind control socket {}", paths.control_socket.display()))?;
    let tcp_listeners = bind_tcp(paths.control_tcp_port).await?;
    tracing::info!(
        socket = %paths.control_socket.display(),
        tcp_port = paths.control_tcp_port,
        tcp_listeners = tcp_listeners.len(),
        "control API listening"
    );

    // Each listener gets its own stop signal, all fanned from the single shutdown
    // future: the Unix edge plus one per LAN-front address family.
    let (unix_stop_tx, unix_stop_rx) = oneshot::channel::<()>();
    let mut tcp_stop_txs = Vec::with_capacity(tcp_listeners.len());
    let mut tcp_tasks = Vec::with_capacity(tcp_listeners.len());
    for listener in tcp_listeners {
        let (tx, rx) = oneshot::channel::<()>();
        tcp_stop_txs.push(tx);
        tcp_tasks.push(tokio::spawn(serve_tcp(listener, tcp_router.clone(), rx)));
    }
    tokio::spawn(async move {
        shutdown.await;
        let _ = unix_stop_tx.send(());
        for tx in tcp_stop_txs {
            let _ = tx.send(());
        }
    });

    let unix = tokio::spawn(serve_unix(unix_listener, unix_router, unix_stop_rx));

    sd_ready();

    // Run until the stop trigger fires, pinging the systemd watchdog on a fixed
    // cadence in between. A wedged async runtime stops pinging and systemd
    // restarts the unit; a healthy daemon keeps the timer fed.
    let mut watchdog = tokio::time::interval(WATCHDOG_PING_INTERVAL);
    watchdog.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // The first immediate tick fires right after READY; consume it so the
    // cadence stays a steady WATCHDOG_PING_INTERVAL.
    watchdog.tick().await;
    let serving = async {
        let _ = unix.await;
        for tcp in tcp_tasks {
            let _ = tcp.await;
        }
    };
    tokio::pin!(serving);
    loop {
        tokio::select! {
            _ = &mut serving => break,
            _ = watchdog.tick() => sd_watchdog(),
        }
    }
    sd_stopping();

    // Stop the IPC readers before exiting so their tasks do not outlive the run.
    state_handle.shutdown().await;
    battery_handle.shutdown().await;
    swarm_handle.shutdown().await;

    // tmpfs cleanup: a stale socket path confuses a probing reader on restart.
    let _ = std::fs::remove_file(&paths.control_socket);
    tracing::info!("control API stopped");
    Ok(())
}
