//! `ados-lora`: the LoRa lane service.
//!
//! Reads `lora:` and `network.regulatory` from the config, resolves the
//! region, opens the configured radio, and runs the beacon scheduler, the
//! receive side and the state sidecars. Exits 0 (nothing to run) when the lane
//! is not enabled. SIGHUP re-reads the config and rebuilds the lane in
//! process; SIGTERM/SIGINT stop it.

mod lane;
mod position;
mod setup;

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::frame::{Frame, Kind, Status};
use crate::governor::max_window_s;
use crate::ledger::Now;
use crate::paths;
use crate::status::{lane_body, LaneState};
use lane::{remember, TxOutcome};
use setup::Built;

pub use lane::{AnyTransport, Lane};

const CONFIG_YAML: &str = "/etc/ados/config.yaml";
const SIDECAR_INTERVAL: Duration = Duration::from_secs(1);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);
const COMPACT_INTERVAL: Duration = Duration::from_secs(3600);
const RETRY_INTERVAL: Duration = Duration::from_secs(5);
/// A status report about the last node heard rides every fourth beacon.
const STATUS_EVERY: u64 = 4;
/// Where a trusted wall clock is announced: systemd-timesyncd's marker, or
/// the agent clock service reporting a GNSS-locked timebase.
const TIMESYNC_MARKER: &str = "/run/systemd/timesync/synchronized";

/// The binary entry point.
pub fn run() -> std::process::ExitCode {
    serve()
}

fn utc_trusted() -> bool {
    if Path::new(TIMESYNC_MARKER).is_file() {
        return true;
    }
    ados_protocol::paths::sidecar("clock")
        .and_then(|row| std::fs::read_to_string(ados_protocol::paths::resolve(row.path)).ok())
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .is_some_and(|v| v["locked"] == true && v["timebase"] == "gnss")
}

/// The clocks now. `mono` is the process-wide monotonic origin mapped onto
/// `CLOCK_MONOTONIC`.
pub(crate) fn now_at(origin: Instant, origin_mono_ns: i64, trusted: bool) -> Now {
    let utc_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    Now {
        mono_ns: origin_mono_ns + origin.elapsed().as_nanos() as i64,
        utc_ms,
        utc_trusted: trusted,
    }
}

#[tokio::main]
async fn serve() -> std::process::ExitCode {
    {
        use ados_protocol::logd::layer::LogdLayer;
        use tracing_subscriber::prelude::*;
        let filter =
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
        let _ = tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer())
            .with(LogdLayer::new("ados-lora"))
            .try_init();
    }
    let config = std::env::var("ADOS_CONFIG").unwrap_or_else(|_| CONFIG_YAML.into());
    let reload = Arc::new(Notify::new());
    {
        let reload = reload.clone();
        tokio::spawn(async move {
            use tokio::signal::unix::{signal, SignalKind};
            let Ok(mut hup) = signal(SignalKind::hangup()) else {
                return;
            };
            while hup.recv().await.is_some() {
                reload.notify_one();
            }
        });
    }
    let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "lora_signal_setup_failed");
            return std::process::ExitCode::FAILURE;
        }
    };
    loop {
        let built = match setup::build(Path::new(&config)).await {
            Ok(Some(b)) => b,
            Ok(None) => {
                tracing::info!("lora_disabled");
                return std::process::ExitCode::SUCCESS;
            }
            Err(e) => {
                tracing::error!(error = %e, "lora_bringup_failed");
                tokio::select! {
                    _ = tokio::time::sleep(RETRY_INTERVAL) => continue,
                    _ = reload.notified() => continue,
                    _ = term.recv() => return std::process::ExitCode::SUCCESS,
                    _ = tokio::signal::ctrl_c() => return std::process::ExitCode::SUCCESS,
                }
            }
        };
        tokio::select! {
            r = run_lane(built) => {
                if let Err(e) = r {
                    tracing::error!(error = %e, "lora_lane_failed");
                    tokio::time::sleep(RETRY_INTERVAL).await;
                }
            }
            _ = reload.notified() => tracing::info!("lora_config_reload"),
            _ = term.recv() => return std::process::ExitCode::SUCCESS,
            _ = tokio::signal::ctrl_c() => return std::process::ExitCode::SUCCESS,
        }
    }
}

/// The beacon interval: the configured one, stretched so beacons use at most
/// 80 % of the tightest duty budget in force on the beacon channel.
pub fn beacon_interval_s(configured_s: u32, charge_us: u64, budget_s: Option<f64>) -> u32 {
    let stretched = budget_s.map_or(0, |b| {
        (charge_us as f64 / 1e6 * 3600.0 / (0.8 * b)).ceil() as u32
    });
    configured_s.max(stretched).max(1)
}

/// Run one built lane until it fails.
async fn run_lane(built: Built) -> std::io::Result<()> {
    let Built {
        mut primary,
        mut bridge,
        lane_state,
        interval_s,
        origin,
        origin_mono_ns,
    } = built;
    let now = || now_at(origin, origin_mono_ns, utc_trusted());
    let beacon_len = crate::validate::BEACON_FRAME_LEN;
    let mut beacons: VecDeque<Value> = VecDeque::new();
    let mut beacon_tick = tokio::time::interval(Duration::from_secs(u64::from(interval_s)));
    let mut sidecar_tick = tokio::time::interval(SIDECAR_INTERVAL);
    let mut heartbeat_tick = tokio::time::interval(HEARTBEAT_INTERVAL);
    let mut compact_tick = tokio::time::interval(COMPACT_INTERVAL);
    compact_tick.tick().await;
    let mut beacons_sent: u64 = 0;
    tracing::info!(interval_s, "lora_lane_up");
    loop {
        tokio::select! {
            ev = primary.transport_recv() => {
                if let Some(entry) = primary.on_rx(ev?, now()) {
                    remember(&mut beacons, entry);
                }
            }
            ev = recv_bridge(&mut bridge) => {
                if let (Some(b), Some(entry)) = (bridge.as_mut(), ev.transpose()?) {
                    if let Some(entry) = b.on_rx(entry, now()) {
                        remember(&mut beacons, entry);
                    }
                }
            }
            _ = beacon_tick.tick() => {
                let beacon = position::current().await;
                let frame = Frame { node_id: primary.node_id, kind: Kind::Beacon, payload: beacon.to_payload() };
                primary.governor.apc_tick(now(), interval_s);
                if let TxOutcome::Refused(r) = primary.transmit(&frame, now).await? {
                    tracing::debug!(reason = r.reason(), "lora_beacon_held");
                }
                if let Some(b) = bridge.as_mut() {
                    b.transmit(&frame, now).await?;
                }
                beacons_sent += 1;
                if beacons_sent.is_multiple_of(STATUS_EVERY) {
                    if let Some((node, rssi, snr)) = primary.last_heard.take().filter(|(_, _, s)| s.is_finite()) {
                        let s = Status { about_node: node, rssi_dbm: rssi.clamp(-128, 127) as i8, snr_db: snr };
                        let f = Frame { node_id: primary.node_id, kind: Kind::Status, payload: s.to_payload() };
                        primary.transmit(&f, now).await?;
                    }
                }
            }
            _ = sidecar_tick.tick() => {
                let n = now();
                let gov = primary.governor.status(beacon_len, n);
                let mut state = lane_state.clone();
                state.rx_frames = primary.rx_frames + bridge.as_ref().map_or(0, |b| b.rx_frames);
                state.rx_crc_errors = primary.rx_crc_errors;
                write_sidecars(&state, &gov, &beacons);
            }
            _ = heartbeat_tick.tick() => {
                let n = now();
                primary.governor.ledger_mut().heartbeat(n)?;
                if let Some(b) = bridge.as_mut() {
                    b.governor.ledger_mut().heartbeat(n)?;
                }
            }
            _ = compact_tick.tick() => {
                let n = now();
                primary.governor.compact(n)?;
                if let Some(b) = bridge.as_mut() {
                    b.governor.compact(n)?;
                }
                tracing::debug!(window_s = max_window_s(primary.governor.plan()), "lora_ledger_compacted");
            }
        }
    }
}

async fn recv_bridge(
    bridge: &mut Option<Lane>,
) -> Option<std::io::Result<crate::transport::RxEvent>> {
    match bridge.as_mut() {
        Some(b) => Some(b.transport_recv().await),
        None => std::future::pending().await,
    }
}

fn write_sidecars(
    state: &LaneState,
    gov: &crate::governor::GovernorStatus,
    beacons: &VecDeque<Value>,
) {
    let body = lane_body(state, gov);
    if let Err(e) = ados_protocol::sidecar::write_json_atomic(&paths::lora_json(), &body, 0o644) {
        tracing::warn!(error = %e, "lora_sidecar_write_failed");
    }
    let list = json!({"v": paths::LORA_SIDECAR_VERSION, "beacons": beacons});
    if let Err(e) =
        ados_protocol::sidecar::write_json_atomic(&paths::lora_beacons_json(), &list, 0o644)
    {
        tracing::warn!(error = %e, "lora_beacons_sidecar_write_failed");
    }
}

#[cfg(test)]
mod tests;
