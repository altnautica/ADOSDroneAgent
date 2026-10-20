//! The `ados-gnss` process: receiver sources, PPS reader, command socket and
//! the 1 Hz sidecar.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use ados_protocol::paths::run_path;
use ados_protocol::shutdown::Shutdown;
use ados_sensorbus::time::mono_ns;
use ados_sensorbus::{Kind, Origin, Publisher, SyncObs, SyncSource};

use crate::config::{GnssConfig, Source};
use crate::pps::PpsSource;
use crate::service::{Service, Shared};
use crate::sources::{self, Ingest};
use crate::{cmdsock, status};

const DEFAULT_CONFIG: &str = "/etc/ados/config.yaml";
const TICK: Duration = Duration::from_secs(1);
const PPS_FETCH_TIMEOUT: Duration = Duration::from_secs(1);

fn init_logging() {
    use ados_protocol::logd::layer::LogdLayer;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::EnvFilter;

    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());
    #[cfg(target_os = "linux")]
    {
        if let Ok(journald) = tracing_journald::layer() {
            let _ = tracing_subscriber::registry()
                .with(EnvFilter::new(&filter))
                .with(journald)
                .with(LogdLayer::new("ados-gnss"))
                .try_init();
            return;
        }
    }
    let _ = tracing_subscriber::registry()
        .with(EnvFilter::new(&filter))
        .with(tracing_subscriber::fmt::layer())
        .with(LogdLayer::new("ados-gnss"))
        .try_init();
}

#[cfg(target_os = "linux")]
fn sd_ready() {
    if let Err(e) = sd_notify::notify(&[sd_notify::NotifyState::Ready]) {
        tracing::debug!(error = %e, "sd_notify READY failed");
    }
}

#[cfg(not(target_os = "linux"))]
fn sd_ready() {}

#[cfg(target_os = "linux")]
fn sd_watchdog() {
    let _ = sd_notify::notify(&[sd_notify::NotifyState::Watchdog]);
}

#[cfg(not(target_os = "linux"))]
fn sd_watchdog() {}

/// The `ados-gnss` binary's entry point.
pub fn run() -> ExitCode {
    init_logging();
    let config = std::env::var_os("ADOS_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG));
    let cfg = GnssConfig::load_from(&config);
    if cfg.receivers.is_empty() {
        tracing::info!("gnss_no_receivers_configured");
        return ExitCode::SUCCESS;
    }
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!(error = %e, "gnss_runtime_failed");
            return ExitCode::FAILURE;
        }
    };
    rt.block_on(serve(cfg));
    ExitCode::SUCCESS
}

async fn serve(cfg: GnssConfig) {
    let shutdown = Shutdown::new();
    {
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            wait_for_signal().await;
            shutdown.trigger();
        });
    }
    let bus = ados_sensorbus::socket_path();
    let publisher = Publisher::spawn(bus.clone(), Origin::Gnss);
    let shared = Service::new(&cfg, mono_ns()).shared();
    tracing::info!(receivers = cfg.receivers.len(), "gnss_service_starting");

    for (idx, r) in cfg.receivers.iter().enumerate() {
        let ingest = Ingest::new(
            shared.clone(),
            idx,
            publisher.clone(),
            r.source != Source::Hub,
        );
        tokio::spawn(sources::run(
            ingest,
            r.source.clone(),
            bus.clone(),
            shutdown.clone(),
        ));
    }
    if let Some(dev) = &cfg.pps_device {
        start_pps(dev, &shared, &publisher, &cfg.receivers[0].id, &shutdown);
    }
    let peers = run_path("linked-peers");
    tokio::spawn(cmdsock::serve(
        shared.clone(),
        run_path("gnss-cmd"),
        peers.clone(),
        shutdown.clone(),
    ));
    sd_ready();

    let sidecar = run_path("gnss");
    let mut write_failing = false;
    let mut tick = tokio::time::interval(TICK);
    loop {
        tokio::select! {
            _ = shutdown.wait() => break,
            _ = tick.tick() => {}
        }
        let targets = status::linked_peer_count(&peers);
        let now = mono_ns();
        let body = {
            let mut svc = shared.lock();
            svc.tick(now);
            svc.status(now, targets)
        };
        match status::write(&sidecar, &body) {
            Err(e) if !write_failing => {
                tracing::warn!(error = %e, "gnss_sidecar_write_failed");
                write_failing = true;
            }
            Ok(()) => write_failing = false,
            _ => {}
        }
        sd_watchdog();
    }
    tracing::info!("gnss_service_stopping");
}

async fn wait_for_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let Ok(mut term) = signal(SignalKind::terminate()) else {
        let _ = tokio::signal::ctrl_c().await;
        return;
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

#[cfg(target_os = "linux")]
fn open_pps(dev: &Path) -> std::io::Result<Box<dyn PpsSource>> {
    Ok(Box::new(crate::pps::DevPps::open(dev)?))
}

#[cfg(not(target_os = "linux"))]
fn open_pps(_dev: &Path) -> std::io::Result<Box<dyn PpsSource>> {
    Err(std::io::Error::other("PPS needs Linux"))
}

fn start_pps(dev: &Path, shared: &Shared, publisher: &Publisher, id: &str, shutdown: &Shutdown) {
    let src = match open_pps(dev) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(device = %dev.display(), error = %e, "gnss_pps_unavailable");
            return;
        }
    };
    let (shared, publisher, id, shutdown) = (
        shared.clone(),
        publisher.clone(),
        id.to_string(),
        shutdown.clone(),
    );
    let spawned = std::thread::Builder::new()
        .name("gnss-pps".into())
        .spawn(move || pps_loop(src, &shared, &publisher, &id, &shutdown));
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "gnss_pps_thread_failed");
    }
}

/// Fetch edges until shutdown; publish each paired pulse as a `sync` pair.
pub fn pps_loop(
    mut src: Box<dyn PpsSource>,
    shared: &Shared,
    publisher: &Publisher,
    id: &str,
    shutdown: &Shutdown,
) {
    while !shutdown.is_fired() {
        let edge = match src.fetch(PPS_FETCH_TIMEOUT) {
            Ok(Some(e)) => e,
            Ok(None) => continue,
            Err(e) => {
                tracing::warn!(error = %e, "gnss_pps_fetch_failed");
                std::thread::sleep(PPS_FETCH_TIMEOUT);
                continue;
            }
        };
        let host_ns = edge.host_ns();
        let Some(utc_ns) = shared.lock().pairer.on_pulse(host_ns) else {
            continue;
        };
        let obs = SyncObs {
            source: SyncSource::GnssPps,
            host_ns,
            source_ns: utc_ns,
            rtt_ns: 0,
            sim_time: false,
        };
        publisher.msgpack(Kind::Sync, id, &obs);
    }
}
