//! The `ados-clock` process: bus broker, tap consumer, 1 Hz publisher.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use ados_protocol::paths::run_path;
use ados_sensorbus::{time::mono_ns, Kind, Origin, Publisher};
use tokio::signal::unix::{signal, SignalKind};
use tokio::task::JoinHandle;

use crate::chrony::{ChronySink, CHRONY_SOCK};
use crate::engine::{write_sidecar, Engine};

/// Cadence of the `clock` frame, the sidecars and the watchdog ping.
pub const TICK: Duration = Duration::from_secs(1);
/// The stream id of the `clock` frames.
pub const CLOCK_SRC: &str = "clock";

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
                .with(LogdLayer::new("ados-clock"))
                .try_init();
            return;
        }
    }
    let _ = tracing_subscriber::registry()
        .with(EnvFilter::new(&filter))
        .with(tracing_subscriber::fmt::layer())
        .with(LogdLayer::new("ados-clock"))
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

/// The `ados-clock` binary's entry point.
pub fn run() -> ExitCode {
    match run_async() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "ados-clock failed");
            ExitCode::FAILURE
        }
    }
}

/// Where the 1 Hz outputs go.
struct Outputs {
    publisher: Publisher,
    clock_json: PathBuf,
    sensors_json: PathBuf,
    /// The sidecar write in flight on the blocking pool (fsync can stall).
    writer: Option<JoinHandle<std::io::Result<()>>>,
    /// Whether the last sidecar write failed (logged once per transition).
    write_failing: bool,
}

impl Outputs {
    async fn publish(&mut self, engine: &Engine, now: u64) {
        self.publisher
            .proto(Kind::Clock, CLOCK_SRC, &engine.clock.sample(now));
        if let Some(writer) = self.writer.take() {
            if !writer.is_finished() {
                // The previous write is still syncing; skip this second.
                tracing::debug!("clock sidecar write still in flight");
                self.writer = Some(writer);
                return;
            }
            let result = writer
                .await
                .unwrap_or_else(|e| Err(std::io::Error::other(e)));
            self.note(result);
        }
        let (clock, sensors) = (engine.clock.report(now), engine.streams.report(now));
        let (clock_json, sensors_json) = (self.clock_json.clone(), self.sensors_json.clone());
        self.writer = Some(tokio::task::spawn_blocking(move || {
            write_sidecar(&clock_json, &clock)?;
            write_sidecar(&sensors_json, &sensors)
        }));
    }

    fn note(&mut self, result: std::io::Result<()>) {
        match result {
            Err(e) if !self.write_failing => {
                tracing::warn!(error = %e, "clock sidecar write failed");
                self.write_failing = true;
            }
            Ok(()) if self.write_failing => {
                tracing::info!("clock sidecar writes recovered");
                self.write_failing = false;
            }
            _ => {}
        }
    }
}

#[tokio::main]
async fn run_async() -> anyhow::Result<()> {
    init_logging();
    let socket = ados_sensorbus::socket_path();
    let (mut tap, stats) = crate::spawn_bus(&socket)?;
    let mut engine = Engine::new(Some(ChronySink::new(CHRONY_SOCK)));
    let mut out = Outputs {
        publisher: Publisher::spawn(socket.clone(), Origin::Host),
        clock_json: run_path("clock"),
        sensors_json: run_path("sensors"),
        writer: None,
        write_failing: false,
    };
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    sd_ready();
    tracing::info!(socket = %socket.display(), "clock_ready");
    let mut last_timebase = None;
    loop {
        tokio::select! {
            _ = sigterm.recv() => { tracing::info!("received SIGTERM"); break; }
            _ = sigint.recv() => { tracing::info!("received SIGINT"); break; }
            Some(frame) = tap.recv() => engine.ingest(&frame, mono_ns()),
            _ = tick.tick() => {
                let now = mono_ns();
                out.publish(&engine, now).await;
                let timebase = engine.clock.timebase(now);
                if last_timebase != Some(timebase) {
                    tracing::info!(?timebase, "clock_timebase");
                    last_timebase = Some(timebase);
                }
                sd_watchdog();
            }
        }
    }
    let frames = stats.frames_in.load(std::sync::atomic::Ordering::Relaxed);
    tracing::info!(
        frames,
        chrony_samples = engine.chrony_sent(),
        "clock stopping"
    );
    remove_socket(&socket);
    Ok(())
}

fn remove_socket(path: &Path) {
    if let Err(e) = std::fs::remove_file(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(error = %e, path = %path.display(), "bus socket remove failed");
        }
    }
}
