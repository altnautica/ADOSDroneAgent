//! Daemon wiring: host services, the reconciler, and the loopback guard.

use super::*;

/// The wired daemon: the reconciler that owns the served sockets, the shared
/// issuer, the control socket, and the two background loops.
pub(super) struct WiredDaemon<H: crate::HostServices> {
    pub(super) reconciler: Arc<PluginReconciler<H>>,
    pub(super) issuer: Arc<TokenIssuer>,
    /// The daemon-lifetime control socket (the on-box config-write reach for the
    /// native `ados-control` plugin-config route, plus the lifecycle pokes a
    /// supervisor sends after an enable or a permission change), and its accept
    /// task. Bound once for the whole daemon, not per plugin.
    pub(super) control: Option<(PathBuf, JoinHandle<()>)>,
    /// The state-poll and token-rotation loops.
    pub(super) loops: (JoinHandle<()>, JoinHandle<()>),
    /// The plugin event bus the host publishes its own lifecycle topics on.
    pub(super) bus: Arc<EventBus>,
}

impl<H: crate::HostServices> WiredDaemon<H> {
    /// Abort every background task and unlink every served socket. New
    /// connections stop immediately; an in-flight connection's
    /// `release_plugin` already runs on disconnect.
    pub(super) fn shutdown(self) {
        crate::vehicle_events::publish_host_event(
            &self.bus,
            crate::vehicle_events::TOPIC_AGENT_SHUTDOWN,
            rmpv::Value::Map(vec![]),
        );
        self.loops.0.abort();
        self.loops.1.abort();
        self.reconciler.shutdown();
        if let Some((path, handle)) = self.control {
            handle.abort();
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// The autonomous-injector identity the plugin host declares when injector
/// arbitration is armed. A PIC arbiter designates this id (or leaves PIC
/// unclaimed) to let the host's guided/MSP commands fly; any other holder
/// refuses them.
pub(super) const INJECTOR_CLIENT_ID: &str = "plugin-host";

/// TTL for the host's injector attestation. A fresh ticket is minted for every
/// router connection and the router verifies it once (the claim is sticky), so
/// this only has to cover the window from connect to first command. Generous
/// so a slow bench setup never expires it mid-arm.
pub(super) const INJECTOR_TICKET_TTL_SECS: i64 = 3600;

/// The agent config path, `ADOS_CONFIG`-overridable like every other surface.
pub(super) fn config_yaml_path() -> PathBuf {
    PathBuf::from(
        std::env::var("ADOS_CONFIG").unwrap_or_else(|_| "/etc/ados/config.yaml".to_string()),
    )
}

/// The pairing file, respecting the same `ADOS_PAIRING_JSON` override the router
/// and the CRSF injector read.
pub(super) fn pairing_json_path() -> PathBuf {
    PathBuf::from(
        std::env::var("ADOS_PAIRING_JSON").unwrap_or_else(|_| "/etc/ados/pairing.json".to_string()),
    )
}

/// Whether `mavlink.injector_arbitration` is set in the agent config.
///
/// DEFAULT FALSE: with it off the host declares no injector, so every command it
/// writes is the never-gated operator path and the router's PIC gate stays
/// inert. On (opt-in on a deployment with a PIC arbiter), the host subjects its
/// autonomous guided/MSP commands to the operator's manual-control claim. A
/// missing/unreadable config reads as off.
pub(super) fn injector_arbitration_armed() -> bool {
    match std::fs::read_to_string(config_yaml_path()) {
        Ok(t) => parse_injector_arbitration(&t),
        Err(_) => false,
    }
}

/// Pure parse of the `mavlink.injector_arbitration` flag from config YAML.
/// Unknown/absent → false; unparseable → false. Separated so the default-off
/// behaviour is unit-testable without touching process env.
pub(super) fn parse_injector_arbitration(yaml: &str) -> bool {
    #[derive(serde::Deserialize, Default)]
    struct Cfg {
        #[serde(default)]
        mavlink: MavlinkFlags,
    }
    #[derive(serde::Deserialize, Default)]
    struct MavlinkFlags {
        #[serde(default)]
        injector_arbitration: bool,
    }
    serde_norway::from_str::<Cfg>(yaml)
        .map(|c| c.mavlink.injector_arbitration)
        .unwrap_or(false)
}

/// Build the real host from a discovered supervisor: the five facades plus the
/// router links, the runtime lookup (install dir + spawn allowlist from each
/// plugin's manifest), and the agent-id lookup bound to `device_id`.
///
/// The MAVLink and MSP links are always wired. Each reconnects on its own, so a
/// router that is not up yet (or restarts later) heals without a host restart;
/// until it is up a send answers `sent: false` with reason `disconnected`.
pub(super) async fn build_host(
    install_dir: PathBuf,
    run_dir: PathBuf,
    state_path: PathBuf,
    device_id: String,
    bus: &Arc<EventBus>,
    vehicle_armed: Arc<VehicleArmedProbe>,
) -> Arc<RealHost> {
    // The state file `vision.read_model` reads resolved model status from, and
    // the vehicle-state socket `telemetry.subscribe` reads snapshots from.
    let mut host = RealHost::new()
        .with_state_path(state_path)
        .with_vehicle_state_socket(run_dir.join("state.sock"));

    // Injector arbitration (default off). When armed, the MAVLink + MSP links
    // declare themselves autonomous injectors on every connection so the
    // router's PIC gate subjects their commands to an operator's manual-control
    // claim. The ticket attests the id against the pairing key (`None` on an
    // unpaired node, where the verifier accepts the asserted id) and is minted
    // per connection, so a reconnect never replays an expired one.
    let armed = injector_arbitration_armed();
    if armed {
        tracing::info!("injector arbitration armed (mavlink.injector_arbitration)");
    }
    let declarations = move || -> Declarations {
        if !armed {
            return Box::new(Vec::new);
        }
        Box::new(|| {
            let ticket = ados_protocol::ws_ticket::mint_scoped_ticket(
                &pairing_json_path(),
                &ados_protocol::ws_ticket::crsf_inject_scope(INJECTOR_CLIENT_ID),
                INJECTOR_TICKET_TTL_SECS,
            );
            vec![ados_protocol::ipc::encode_injector_declaration(
                INJECTOR_CLIENT_ID,
                ticket.as_deref(),
            )]
        })
    };

    // A link that declares an injector also asks the router's PIC gate before
    // queueing, so a refused command answers `pic_refused` instead of `sent`.
    let link = |sock: &str, wire: Wire| {
        let link = FrameLink::spawn(run_dir.join(sock), wire, declarations());
        if armed {
            link.with_pic_gate(PicGate::new(INJECTOR_CLIENT_ID))
        } else {
            link
        }
    };

    // (a) MAVLink and MSP router links. MSP carries raw bytes for a Betaflight /
    //     iNav FC; on a node with no MSP FC its link simply stays disconnected.
    let mavlink = Arc::new(link("mavlink.sock", Wire::MavlinkSocket));
    // The host is the only publisher of the public vehicle topics; it derives
    // them from the flight controller's own frames on the router link.
    crate::vehicle_events::spawn_vehicle_events(
        Arc::clone(bus),
        mavlink.subscribe(),
        host.fc_identity(),
        vehicle_armed,
    );
    host = host
        .with_mavlink(mavlink)
        .with_msp(Arc::new(link("msp.sock", Wire::Raw)));

    // (b) Vision client: a self-healing link to the engine's socket, so the
    //     vision request methods proxy to it and the frame-descriptor stream
    //     arms whenever the engine is up. It reconnects forever, so an engine
    //     that starts after the host or restarts under it heals by itself;
    //     while it is down the request methods answer a transient error.
    host = host.with_vision(Arc::new(VisionClient::spawn(run_dir.join("vision.sock"))));

    // (c) Runtime lookup: plugin_id -> (install_dir, subprocess_spawn allowlist).
    //     The install dir is the unpacked plugin dir; the allowlist is read from
    //     that plugin's manifest agent.subprocess_spawn. A plugin with no
    //     manifest / no agent block resolves to None (the handler then returns
    //     the structured not_available shape).
    let lookup_install_dir = install_dir.clone();
    host = host.with_runtime_lookup(Box::new(move |plugin_id: &str| {
        let manifest = read_plugin_manifest(&lookup_install_dir, plugin_id)?;
        let agent = manifest.agent.as_ref()?;
        let allowlist: BTreeSet<String> = agent.subprocess_spawn.iter().cloned().collect();
        Some((lookup_install_dir.join(plugin_id), allowlist))
    }));

    // (d) Agent-id lookup: the paired device id, read once at startup (see
    //     `Paths::device_id_file`). This isolates a drone-scoped config write
    //     per drone instead of silently collapsing every drone write to
    //     global. The id is the same for every plugin on one drone.
    if device_id.is_empty() {
        tracing::warn!(
            "device id not resolved; drone-scoped plugin config will fall back to global"
        );
    } else {
        tracing::info!(device_id = %device_id, "plugin config drone scope bound to device id");
    }
    host = host.with_agent_id_lookup(Box::new(move |_plugin_id: &str| device_id.clone()));

    // (e) Config persistence: a 0600 JSON store so plugin config survives a
    //     plugin-host restart instead of living only in memory.
    host = host.with_config_persistence(config_store_path());

    Arc::new(host)
}

/// The plugin-config persistence file. Lives under the agent's etc dir so it
/// survives a restart; overridable via `ADOS_PLUGIN_CONFIG_PATH` for tests.
pub(super) fn config_store_path() -> PathBuf {
    PathBuf::from(
        std::env::var("ADOS_PLUGIN_CONFIG_PATH")
            .unwrap_or_else(|_| "/etc/ados/plugin-config.json".to_string()),
    )
}

/// Wire the daemon: build the host, construct the server, attach the token
/// mint, bind the control socket, and start the reconciler plus its two
/// background loops. Factored out of `main` so the smoke test exercises the
/// full wiring without `main()`.
///
/// `state_path` is the plugin state file the reconciler watches; the socket dir
/// is where the per-plugin sockets bind; the control dir holds the root-only
/// control socket; the install dir / run dir feed the host lookups.
/// `secret_path` is the persisted HMAC secret the issuer is built from; the
/// same file feeds the unit-generation path, so a runner's token verifies here.
pub(super) async fn wire(
    paths: &Paths,
    vehicle_armed: Arc<VehicleArmedProbe>,
) -> WiredDaemon<RealHost> {
    let state_path = paths.state_path.clone();
    let install_dir = paths.install_dir.clone();
    let socket_dir = paths.socket_dir.clone();
    let control_dir = paths.control_dir.clone();
    let run_dir = paths.run_dir.clone();
    let secret_path = paths.token_secret.as_path();
    // Build the issuer from the persisted secret so a runner started by its own
    // unit can present a token this daemon verifies. A failure to read/create
    // the secret falls back to a per-process random key (the in-process smoke
    // test still works; cross-process verify degrades, surfaced by the warn).
    let issuer = Arc::new(match crate::shared_issuer(secret_path) {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(
                path = %secret_path.display(),
                error = %e,
                "persisted token secret unavailable; falling back to a per-process key \
                 (runner tokens will not verify cross-process)"
            );
            TokenIssuer::new_random()
        }
    });
    let bus = Arc::new(EventBus::new());
    // The paired device id scopes each plugin's per-drone data dir, written into
    // the runner's env file by the mint, and its drone-scoped config. Empty on
    // an unpaired node (node scope).
    let device_id = ados_protocol::identity::device_id_at(&paths.device_id_file, None);
    let host = build_host(
        install_dir.clone(),
        run_dir,
        state_path.clone(),
        device_id.clone(),
        &bus,
        vehicle_armed,
    )
    .await;
    let mint = Arc::new(TokenMint::new(
        issuer.clone(),
        state_path.clone(),
        socket_dir.clone(),
        paths.data_root.clone(),
        device_id,
    ));

    // Build the per-plugin server first so the control socket can share its
    // invoke registry (the host->plugin tool.invoke seam). The server holds a
    // clone of the host Arc; the original stays available for the control
    // socket. The mint rides on the server so a request arriving on an aged-out
    // token is answered with a re-mint instead of `token_expired`.
    let server = Arc::new(
        PluginIpcServer::new(&socket_dir, issuer.clone(), Arc::clone(&bus), host.clone())
            .with_token_mint(mint.clone()),
    );

    let reconciler = Arc::new(
        PluginReconciler::new(server.clone(), mint, state_path, install_dir).with_unit_control(
            Arc::new(DaemonUnits(paths.clone())),
            paths.run_dir.join("state.sock"),
            paths.run_dir.clone(),
        ),
    );

    // The on-box control socket: the native `ados-control` plugin routes reach
    // this daemon through it — a GCS skill toggle / per-drone config write for a
    // plugin the writer is not, a `tool.invoke` that runs one of a plugin's MCP
    // tools on its live connection, and the lifecycle pokes a supervisor sends
    // after an enable (`plugin.reconcile`) or a permission change
    // (`token.rotate`). Bound once for the whole daemon. A bind failure is
    // non-fatal: the reconciler's own poll still picks every change up within
    // one interval, so the loss is latency, not correctness.
    let lifecycle: Arc<dyn crate::LifecycleControl> = reconciler.clone();
    let control = match crate::serve_control(
        host.clone(),
        server.invoke_registry(),
        Some(lifecycle),
        control_dir,
    ) {
        Ok((path, handle)) => {
            tracing::info!(socket = %path.display(), "serving plugin-host control socket");
            Some((path, handle))
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "failed to bind plugin-host control socket; GCS plugin config writes, \
                 tool invocations and lifecycle pokes will fall back to the state poll"
            );
            None
        }
    };

    // First pass, then the maintenance loops. The first pass is what boot
    // serving used to be; the loops are what makes a later enable, a revoke and
    // an expiring token self-correcting.
    let first = reconciler.reconcile();
    tracing::info!(
        started = first.started,
        serving = first.serving,
        "initial plugin socket reconcile"
    );
    let loops = crate::reconcile::spawn_loops(reconciler.clone());

    WiredDaemon {
        reconciler,
        issuer,
        control,
        loops,
        bus,
    }
}

/// Render the loopback-guard ruleset for the current install set: every
/// local destination dropped for the plugin group, with each installed
/// plugin's own declared listeners excepted for its uid only.
pub(super) fn guard_ruleset(supervisor: &PluginSupervisor) -> String {
    let ws_port = std::fs::read_to_string(config_yaml_path())
        .map(|y| loopback_guard::configured_ws_port(&y))
        .unwrap_or(loopback_guard::DEFAULT_MAVLINK_WS_PORT);
    loopback_guard::render_ruleset(
        &supervisor.loopback_guard_exceptions(),
        &loopback_guard::agent_tcp_ports(ws_port),
    )
}

/// A lifecycle controller over this process's layout, with the board gates.
pub(super) fn production_supervisor(paths: &Paths) -> PluginSupervisor {
    let (board_id, board_tier) =
        PluginSupervisor::board_identity(&paths.run_dir.join("board.json"));
    PluginSupervisor::production(paths.clone(), board_id, agent_version())
        .with_profile(ados_config::node_profile())
        .with_board_tier(board_tier)
        .with_accounts(crate::plugin_account::local_accounts())
}

/// The reconciler's unit side, backed by a fresh lifecycle controller per
/// call so it acts on the state on disk now.
pub(super) struct DaemonUnits(pub(super) Paths);

impl crate::reconcile::UnitControl for DaemonUnits {
    fn refresh_units(&self) {
        let mut supervisor = production_supervisor(&self.0);
        if let Err(e) = supervisor.discover() {
            tracing::warn!(error = %e, "plugin discovery failed before a unit refresh");
        }
        supervisor.refresh_all_units();
    }

    fn stop_plugin_units(&self, plugin_id: &str) {
        production_supervisor(&self.0).stop_tampered(plugin_id);
    }
}

/// `guard-load`: ensure the plugin groups exist, reload the guard, and exit
/// non-zero when it did not load, so the plugin unit whose pre-start this is
/// fails closed. The verdict is not recorded here: this runs in the plugin
/// unit's mount namespace, where the run dir is the unit's read-only tmpfs
/// (see [`loopback_guard::reload`]); the daemon's record stands.
pub(super) fn guard_load() -> Result<()> {
    let paths = Paths::from_env();
    if let Err(e) = crate::plugin_account::ensure_groups() {
        anyhow::bail!("plugin groups unavailable: {e}");
    }
    let mut supervisor = production_supervisor(&paths);
    if let Err(e) = supervisor.discover() {
        tracing::warn!(error = %e, "plugin discovery failed; loading the guard without listener exceptions");
    }
    let state = loopback_guard::reload(&guard_ruleset(&supervisor));
    if !state.active {
        anyhow::bail!("plugin loopback guard did not load: {}", state.reason);
    }
    Ok(())
}
