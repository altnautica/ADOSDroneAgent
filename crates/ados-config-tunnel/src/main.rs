//! `ados-tunnel-config` binary — the config-over-radio substrate service.
//!
//! One binary, two roles by profile: on a **drone** it runs the terminator
//! (serves `/api/config` requests arriving over the bearer); on a **ground
//! station** it runs the injector + its command socket (emits requests +
//! awaits replies). It idles harmlessly with an honest `disabled` sidecar when
//! the channel is not opted in or the profile does not run it, and reloads its
//! config in place on SIGHUP.
//!
//! The channel carries CONFIG request/response ONLY — never armed-flight
//! command authority — and ships inert (default off). See the crate docs.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{watch, Mutex, Notify};

use ados_config_tunnel::cmdsock;
use ados_config_tunnel::config::{resolve_profile, TunnelChannelConfig};
use ados_config_tunnel::config_client::HttpConfigClient;
use ados_config_tunnel::injector::Injector;
use ados_config_tunnel::paths::{run_path, write_sidecar, LOCAL_CONFIG_BASE_URL};
use ados_config_tunnel::sidecar::{build_sidecar, ChannelState, SidecarInputs};
use ados_config_tunnel::stats::Counters;
use ados_config_tunnel::terminator::run_terminator;
use ados_config_tunnel::transport::UdpTunnelTransport;

const CONFIG_YAML: &str = "/etc/ados/config.yaml";
const PROFILE_CONF: &str = "/etc/ados/profile.conf";
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
const IDLE_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() {
    {
        use ados_protocol::logd::layer::LogdLayer;
        use tracing_subscriber::prelude::*;
        let filter =
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
        let _ = tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer())
            .with(LogdLayer::new("ados-tunnel-config"))
            .try_init();
    }
    tracing::info!("tunnel_config_service_starting");

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        wait_for_shutdown().await;
        let _ = shutdown_tx.send(true);
    });

    let reload = Arc::new(Notify::new());
    #[cfg(unix)]
    {
        let reload = reload.clone();
        tokio::spawn(async move {
            use tokio::signal::unix::{signal, SignalKind};
            let Ok(mut hup) = signal(SignalKind::hangup()) else {
                return;
            };
            while hup.recv().await.is_some() {
                tracing::info!("tunnel_config_reload_signal");
                reload.notify_one();
            }
        });
    }

    run_until_shutdown(
        Path::new(CONFIG_YAML),
        Path::new(PROFILE_CONF),
        shutdown_rx,
        reload,
    )
    .await;
    tracing::info!("tunnel_config_service_stopped");
}

#[derive(Debug, PartialEq, Eq)]
enum RunExit {
    Shutdown,
    Reload,
}

async fn run_until_shutdown(
    config_path: &Path,
    profile_conf: &Path,
    shutdown: watch::Receiver<bool>,
    reload: Arc<Notify>,
) {
    loop {
        if *shutdown.borrow() {
            return;
        }
        let cfg = TunnelChannelConfig::load_from(config_path);
        let profile = resolve_profile(config_path, profile_conf);
        match run_service(&cfg, &profile, shutdown.clone(), reload.clone()).await {
            RunExit::Shutdown => return,
            RunExit::Reload => tracing::info!("tunnel_config_reloaded"),
        }
    }
}

async fn run_service(
    cfg: &TunnelChannelConfig,
    profile: &str,
    shutdown: watch::Receiver<bool>,
    reload: Arc<Notify>,
) -> RunExit {
    // Master opt-in gate: idle harmlessly with an honest disabled sidecar.
    if !cfg.enabled {
        tracing::info!("tunnel_config_disabled");
        return idle_disabled(shutdown, reload).await;
    }
    match profile {
        "drone" => run_drone(cfg, shutdown, reload).await,
        "ground-station" => run_ground_station(cfg, shutdown, reload).await,
        other => {
            // Defensive: the unit is already profile-gated. An unknown profile
            // idles rather than binding a bearer it should not.
            tracing::warn!(profile = other, "tunnel_config_unrecognised_profile_idle");
            idle_disabled(shutdown, reload).await
        }
    }
}

/// Idle until shutdown/reload, refreshing a `disabled` sidecar so a
/// staleness-gated reader can tell a live idle service from a dead one.
async fn idle_disabled(mut shutdown: watch::Receiver<bool>, reload: Arc<Notify>) -> RunExit {
    let counters = Arc::new(Counters::default());
    let inputs = SidecarInputs {
        state: ChannelState::Disabled,
        enabled: false,
        command_enabled: false,
        rx_port: None,
        tx_port: None,
        counters: counters.snapshot(),
    };
    write_current_sidecar(&inputs);
    let mut tick = tokio::time::interval(IDLE_REFRESH_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => {
                if *shutdown.borrow() { return RunExit::Shutdown; }
            }
            _ = reload.notified() => return RunExit::Reload,
            _ = tick.tick() => write_current_sidecar(&inputs),
        }
    }
}

async fn run_drone(
    cfg: &TunnelChannelConfig,
    shutdown: watch::Receiver<bool>,
    reload: Arc<Notify>,
) -> RunExit {
    let transport = match UdpTunnelTransport::bind(cfg.rx_port, cfg.tx_port).await {
        Ok(t) => Arc::new(t),
        Err(e) => {
            tracing::warn!(error = %e, rx_port = cfg.rx_port, "tunnel_config_bind_failed_idle");
            return idle_disabled(shutdown, reload).await;
        }
    };
    let client = Arc::new(HttpConfigClient::new(LOCAL_CONFIG_BASE_URL));
    let counters = Arc::new(Counters::default());

    let (pass_stop_tx, pass_stop_rx) = watch::channel(false);
    let hb = spawn_heartbeat(
        ChannelState::Terminator,
        cfg.clone(),
        counters.clone(),
        None,
        pass_stop_rx,
    );

    let reloaded = run_terminator(
        transport,
        cfg.command_enabled,
        client,
        counters,
        shutdown,
        reload,
    )
    .await;

    let _ = pass_stop_tx.send(true);
    let _ = hb.await;
    if reloaded {
        RunExit::Reload
    } else {
        RunExit::Shutdown
    }
}

async fn run_ground_station(
    cfg: &TunnelChannelConfig,
    mut shutdown: watch::Receiver<bool>,
    reload: Arc<Notify>,
) -> RunExit {
    let transport = match UdpTunnelTransport::bind(cfg.rx_port, cfg.tx_port).await {
        Ok(t) => Arc::new(t),
        Err(e) => {
            tracing::warn!(error = %e, rx_port = cfg.rx_port, "tunnel_config_bind_failed_idle");
            return idle_disabled(shutdown, reload).await;
        }
    };
    let counters = Arc::new(Counters::default());
    let (pass_stop_tx, pass_stop_rx) = watch::channel(false);
    let injector = Injector::spawn(transport, counters.clone(), pass_stop_rx.clone());

    let latest_status = Arc::new(Mutex::new(build_sidecar(&SidecarInputs {
        state: ChannelState::Injector,
        enabled: cfg.enabled,
        command_enabled: cfg.command_enabled,
        rx_port: Some(cfg.rx_port),
        tx_port: Some(cfg.tx_port),
        counters: counters.snapshot(),
    })));
    let hb = spawn_heartbeat(
        ChannelState::Injector,
        cfg.clone(),
        counters,
        Some(latest_status.clone()),
        pass_stop_rx,
    );

    let cmd_state = cmdsock::CmdState {
        injector,
        enabled: cfg.enabled,
        command_enabled: cfg.command_enabled,
        latest_status,
    };
    let sock = run_path("tunnel-config-cmd.sock");

    let exit = tokio::select! {
        biased;
        _ = shutdown.changed() => {
            if *shutdown.borrow() { RunExit::Shutdown } else { RunExit::Reload }
        }
        _ = reload.notified() => RunExit::Reload,
        r = cmdsock::serve(cmd_state, Path::new(&sock)) => {
            if let Err(e) = r {
                tracing::warn!(error = %e, "tunnel_config_cmdsock_ended");
            }
            RunExit::Shutdown
        }
    };
    let _ = pass_stop_tx.send(true);
    let _ = hb.await;
    exit
}

/// Spawn the ~1 Hz sidecar heartbeat for a pass. When `latest_status` is set
/// (the GS role), each write also refreshes the in-memory status the command
/// socket's `status` op serves. Stops when `pass_stop` flips true.
fn spawn_heartbeat(
    state: ChannelState,
    cfg: TunnelChannelConfig,
    counters: Arc<Counters>,
    latest_status: Option<Arc<Mutex<serde_json::Value>>>,
    mut pass_stop: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(HEARTBEAT_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = pass_stop.changed() => {
                    if *pass_stop.borrow() { return; }
                }
                _ = tick.tick() => {
                    let inputs = SidecarInputs {
                        state,
                        enabled: cfg.enabled,
                        command_enabled: cfg.command_enabled,
                        rx_port: Some(cfg.rx_port),
                        tx_port: Some(cfg.tx_port),
                        counters: counters.snapshot(),
                    };
                    let body = build_sidecar(&inputs);
                    if let Some(status) = &latest_status {
                        *status.lock().await = body.clone();
                    }
                    if let Err(e) = write_sidecar(&run_path("tunnel-config.json"), &body) {
                        tracing::debug!(error = %e, "tunnel_config_sidecar_write_failed");
                    }
                }
            }
        }
    })
}

fn write_current_sidecar(inputs: &SidecarInputs) {
    let body = build_sidecar(inputs);
    // `run_path` resolves TUNNEL_CONFIG_STATS_JSON's basename under the
    // (env-overridable) run dir.
    if let Err(e) = write_sidecar(&run_path("tunnel-config.json"), &body) {
        tracing::debug!(error = %e, "tunnel_config_sidecar_write_failed");
    }
}

async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
