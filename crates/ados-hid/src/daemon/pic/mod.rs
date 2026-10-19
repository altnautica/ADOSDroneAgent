//! `ados-pic` daemon.
//!
//! The sole owner of the PIC arbiter state. Serves the arbiter over the
//! Unix-domain control socket (the IPC seam other processes reach it through),
//! runs the session watchdog (auto-release after the heartbeat timeout), and
//! reads the front-panel GPIO buttons. Modelled on `ados-supervisor`'s main.
//!
//! The button reader runs only on a board whose sidecar binds buttons it can
//! map to GPIO lines (see [`crate::buttons::board`]); otherwise it is skipped
//! with a logged reason and the daemon stays up for the PIC seam alone, which
//! every ground station needs regardless of whether it has physical buttons.

use std::path::Path;
use std::sync::{Arc, RwLock};

use anyhow::Result;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::Mutex;

use crate::buttons::{self, default_button_mapping};
use crate::buttons_ipc::{self, BUTTONS_SOCK};
use crate::eventbus::ButtonEventBus;
use crate::paths::pic_state_json;
use crate::pic::PicArbiter;
use crate::pic_ipc::{self, PIC_SOCK};

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
                .with(LogdLayer::new("ados-pic"))
                .try_init();
            return;
        }
    }

    let _ = tracing_subscriber::registry()
        .with(EnvFilter::new(&filter))
        .with(tracing_subscriber::fmt::layer())
        .with(LogdLayer::new("ados-pic"))
        .try_init();
}

/// systemd readiness. No-op off Linux / outside a Type=notify unit.
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

/// The `ados-pic` binary's entry point.
pub fn run() -> std::process::ExitCode {
    super::exit_code(serve())
}

#[tokio::main]
async fn serve() -> Result<()> {
    init_logging();
    tracing::info!("ados-pic starting");

    let arbiter: pic_ipc::SharedArbiter = Arc::new(Mutex::new(PicArbiter::new()));

    // The button fanout: the GPIO reader publishes classified presses here, and
    // the display/OLED layer subscribes over the control socket
    // (`subscribe_buttons`). Front-panel presses reach a consumer rather than
    // only the journal.
    let button_bus = ButtonEventBus::new();

    // The PIC state sidecar (`/run/ados/pic-state.json`): the IPC server mirrors
    // the arbiter snapshot here after every state-changing op, and the watchdog
    // below rewrites it on every tick (idle included), so a reader (the native
    // status route, the crsf hybrid merge, the injector gates, the display
    // layer) sees the live holder without a socket round-trip and never ages
    // a healthy idle arbiter past the staleness window.
    let sidecar_path = Arc::new(pic_state_json());

    // The IPC seam: the FastAPI /pic/* routes + the kiosk reach the arbiter
    // here, and the display layer streams button presses here. Always served —
    // every ground station needs it.
    {
        let arbiter = arbiter.clone();
        let button_bus = button_bus.clone();
        let sidecar_path = sidecar_path.clone();
        tokio::spawn(async move {
            let path = Path::new(PIC_SOCK);
            if let Err(e) = pic_ipc::serve(arbiter, button_bus, path, Some(sidecar_path)).await {
                tracing::error!(error = %e, "pic control socket exited");
            }
        });
    }

    // The dedicated button fanout: the same classified presses, streamed on their
    // own socket so the HDMI cockpit relay can subscribe without touching the PIC
    // arbiter seam. Shares the one button bus, so short/long/cancel + the config
    // mapping stay single-source in the reader/classifier below.
    // Front-panel buttons: skip-clean when the board has no GPIO chip. The live
    // mapping handle is shared with the SIGHUP task so a remap is seen by the
    // next release.
    let mapping = Arc::new(RwLock::new(load_button_mapping()));

    {
        // The injection classifier shares the same mapping handle as the GPIO
        // reader, so a synthetic press resolves the same action a real one would
        // and a SIGHUP remap reaches both. It stays inert unless the operator
        // sets ADOS_BUTTONS_ALLOW_INJECT=1 (the op checks the env itself).
        let inject = Arc::new(std::sync::Mutex::new(
            crate::buttons::PressClassifier::with_pins(
                mapping.clone(),
                buttons::board_pins_or_default(Path::new(buttons::BOARD_SIDECAR)),
            ),
        ));
        let button_bus = button_bus.clone();
        tokio::spawn(async move {
            let path = Path::new(BUTTONS_SOCK);
            if let Err(e) = buttons_ipc::serve_with_inject(button_bus, Some(inject), path).await {
                tracing::error!(error = %e, "button event socket exited");
            }
        });
    }

    spawn_button_reader(mapping.clone(), button_bus.clone());

    sd_ready();
    tracing::info!(sock = PIC_SOCK, "ados-pic ready");

    // Session watchdog: auto-release a stale PIC after the heartbeat timeout and
    // refresh the state sidecar on every tick.
    let mut tick = pic_ipc::watchdog_interval();

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sighup = signal(SignalKind::hangup())?;

    loop {
        tokio::select! {
            _ = tick.tick() => {
                if let Some(client) = pic_ipc::watchdog_pass(&sidecar_path, &arbiter).await {
                    tracing::info!(client_id = %client, "pic auto-released on heartbeat timeout");
                }
                sd_watchdog();
            }
            _ = sighup.recv() => {
                tracing::info!("SIGHUP: reloading button mapping");
                let merged = load_button_mapping();
                if let Ok(mut m) = mapping.write() {
                    *m = merged;
                }
            }
            _ = sigterm.recv() => {
                tracing::info!("received SIGTERM");
                break;
            }
            _ = sigint.recv() => {
                tracing::info!("received SIGINT");
                break;
            }
        }
    }

    tracing::info!("ados-pic stopped");
    // The button reader runs on a blocking thread that may be parked on a
    // synchronous GPIO read; returning from main would drop the runtime and wait
    // for that thread to finish, which never happens, so the unit would sit in
    // "deactivating" until the systemd stop-timeout SIGKILLs it. Exit the process
    // directly on the shutdown signal instead — there is no shutdown-only state to
    // flush (the sidecar is mirrored after each op, and the socket is removed on
    // the next start), so an immediate exit is clean.
    std::process::exit(0);
}

/// Load + merge `ground_station.ui.buttons.mapping` over the defaults of the
/// node's resolved profile (a visor gets the visor table). A missing config or
/// block falls back to the defaults verbatim. Re-resolved on every SIGHUP.
fn load_button_mapping() -> std::collections::HashMap<String, String> {
    let profile = ados_config::node_profile().replace('-', "_");
    match read_buttons_block() {
        Some(block) => buttons::merge_mapping(&profile, &block),
        None => default_button_mapping(&profile),
    }
}

/// Read the `ground_station.ui.buttons.mapping` node from the agent config, if
/// present. Best-effort: any read/parse error yields `None` and the caller uses
/// the defaults.
fn read_buttons_block() -> Option<serde_norway::Value> {
    let path = std::env::var("ADOS_CONFIG").unwrap_or_else(|_| "/etc/ados/config.yaml".to_string());
    let text = std::fs::read_to_string(path).ok()?;
    let root: serde_norway::Value = serde_norway::from_str(&text).ok()?;
    root.get("ground_station")?
        .get("ui")?
        .get("buttons")?
        .get("mapping")
        .cloned()
}

/// How long the reader waits for the supervisor to publish the board sidecar
/// before giving up on buttons for this run.
#[cfg(target_os = "linux")]
const SIDECAR_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Spawn the button reader on a blocking task. It waits for the board sidecar,
/// resolves the board's button lines, and skips with a logged reason when the
/// board binds none it can read. Only the chip-open is hardware-coupled; the
/// classification runs through the shared [`PressClassifier`]. Each classified
/// press is published to `button_bus` so the display/OLED subscriber acts on it
/// (and logged for the journal).
#[cfg(target_os = "linux")]
fn spawn_button_reader(
    mapping: Arc<RwLock<std::collections::HashMap<String, String>>>,
    button_bus: ButtonEventBus,
) {
    use crate::buttons::{button_lines, ButtonEvent, ButtonsSkip, PressClassifier, BOARD_SIDECAR};
    use crate::eventbus::ButtonBusEvent;

    tokio::task::spawn_blocking(move || {
        let started = std::time::Instant::now();
        let text = loop {
            match std::fs::read_to_string(BOARD_SIDECAR) {
                Ok(text) => break Some(text),
                Err(_) if started.elapsed() < SIDECAR_WAIT => {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
                Err(_) => break None,
            }
        };
        let lines = text
            .ok_or(ButtonsSkip::SidecarUnreadable)
            .and_then(|text| button_lines(&text));
        let lines = match lines {
            Ok(lines) => lines,
            Err(skip) => {
                match &skip {
                    ButtonsSkip::NumberingUnknown { soc } => {
                        tracing::warn!(soc = %soc, "{}", skip.event())
                    }
                    ButtonsSkip::Reserved { gpio } => tracing::warn!(gpio, "{}", skip.event()),
                    _ => tracing::info!(sidecar = BOARD_SIDECAR, "{}", skip.event()),
                }
                return;
            }
        };
        let pins = lines.map(|l| l.gpio);
        let mut classifier = PressClassifier::with_pins(mapping, pins);
        tracing::info!(
            lines = ?lines.map(|l| (l.chip, l.line)),
            gpio = ?pins,
            "button reader started"
        );
        let on_event = |ev: ButtonEvent| {
            let label = buttons::pin_label(&pins, ev.pin);
            tracing::info!(
                pin = ev.pin,
                label = %label,
                kind = ev.kind.as_str(),
                action = ?ev.action,
                "button event"
            );
            // Fan out to the display/OLED consumer over the control socket.
            button_bus.publish(ButtonBusEvent {
                button: ev.pin,
                label,
                kind: ev.kind.as_str(),
                action: ev.action.clone(),
                timestamp_ms: ev.timestamp_ms,
            });
        };
        if let Err(e) = buttons::run_event_loop(&lines, &mut classifier, on_event) {
            // A read error here is not fatal to the PIC seam; log and let the
            // daemon stay up serving the socket.
            tracing::warn!(error = %e, "button reader exited");
        }
    });
}

/// Off Linux there is no character-device GPIO; the reader is a no-op so the
/// daemon still builds and serves the PIC seam on a dev host.
#[cfg(not(target_os = "linux"))]
fn spawn_button_reader(
    _mapping: Arc<RwLock<std::collections::HashMap<String, String>>>,
    _button_bus: ButtonEventBus,
) {
    tracing::debug!("button reader unavailable off Linux");
}
