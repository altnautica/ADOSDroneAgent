//! The receive-plane run loops: the stdout stats reader and the stdout-silence
//! zombie watchdog.
//!
//! `stats_reader_loop` reads `wfb_rx` stdout line-by-line, feeds the link
//! monitor, updates the shared counter + LinkStats + the stdout-liveness stamp,
//! and writes the ground sidecar on every parsed line. `zombie_watchdog`
//! terminates the data RX when its per-second stats stream stalls while the
//! process is alive (process-liveness alone is never proof of work).

use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use tokio::sync::Mutex;

use ados_protocol::logd::emitter::IngestEmitter;
use ados_radio::config::WfbConfig;
use ados_radio::link_quality::{LinkQualityMonitor, LinkStats};

use crate::watchdog::{Clock, SharedRxHealth, RX_HEALTH_SILENCE_THRESHOLD_S};

use super::args::{RX_HEALTH_POLL_INTERVAL_S, STATE_ACTIVE, STATE_SEARCHING};
use super::seams::{live_channel, DataRxHandle, SharedValidCounter};
use super::stats::{build_gs_stats, json_object_to_fields, GsChannelTruth, GsRegSnapshot};

/// Read `wfb_rx` stdout line-by-line, feed the link monitor, update the shared
/// counter + LinkStats + the stdout-liveness stamp, and write the ground
/// `wfb-stats.json` sidecar on every parsed line. Ends on EOF (process death)
/// or task abort.
#[allow(clippy::too_many_arguments)]
pub async fn stats_reader_loop(
    stdout: tokio::process::ChildStdout,
    counter: SharedValidCounter,
    link: Arc<Mutex<LinkStats>>,
    last_stdout_at: Arc<Mutex<f64>>,
    clock: Arc<dyn Clock>,
    interface: String,
    channel: u8,
    rendezvous: u8,
    reg: GsRegSnapshot,
    cfg: WfbConfig,
    chipset: Option<String>,
    injection_ok: bool,
    health: Option<SharedRxHealth>,
    zombie_kills: Arc<AtomicU32>,
    ingest: Option<IngestEmitter>,
) {
    use tokio::io::AsyncBufReadExt;
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    let mut mon = LinkQualityMonitor::new();
    // Last successfully-read live channel; seeded to the operating channel so a
    // momentary `iw info` failure keeps reporting the last-known live value.
    let mut last_live_channel = channel;
    while let Ok(Some(line)) = lines.next_line().await {
        *last_stdout_at.lock().await = clock.monotonic();
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let now_iso = now_iso();
        if let Some(snap) = mon.feed_line(line, &now_iso) {
            // Per-interval valid-decode count feeds the cumulative counter the
            // watchdog/acquirer poll.
            counter.add(snap.packets_received);
            let valid_pps = snap.packets_received as f64; // stats interval = 1 s
            let video_bps = snap.bitrate_kbps as f64 * 1000.0 / 8.0;
            // Lock-state surface: decoding valid video on the current channel ==
            // locked even when no sweep ran. This is the GS received-side proof.
            let (channel_locked, acquire_state) = if snap.packets_received > 0 {
                (true, "locked")
            } else {
                (false, "searching")
            };
            // Top-level lifecycle: "active" ONLY when the RX is actually decoding
            // data. wfb_rx emits a PKT line every second even when it is hearing
            // nothing, so producing stats lines is NOT proof of a working link —
            // hardcoding "active" here made a stone-deaf ground station read as
            // connected. When not decoding, report "searching"; the finer CAUSE
            // (deaf / mis_keyed / jammed) is carried in snap.link_diag.
            let state = if snap.packets_received > 0 {
                STATE_ACTIVE
            } else {
                STATE_SEARCHING
            };
            // Pull the live receive-health counters the watchdogs produce so the
            // sidecar carries real values rather than the previous hardcoded
            // zeros. Absent in tests, where the kills/silence default to zero.
            let (reacquire_kills, rx_silent_seconds) = match &health {
                Some(h) => (h.reacquire_kills(), h.silent_seconds().await),
                None => (0, None),
            };
            let rx_zombie_kills = zombie_kills.load(Ordering::SeqCst);
            *link.lock().await = snap.clone();
            // Truthful channel: read the LIVE interface channel (the acquirer
            // sweep can land it away from the configured/operating channel), with
            // the last-known value held through a transient read failure.
            if let Some(live) = live_channel(&interface).await {
                last_live_channel = live;
            }
            let channels = GsChannelTruth {
                actual: last_live_channel,
                rendezvous,
                operating: channel,
            };
            let payload = build_gs_stats(
                &snap,
                &interface,
                chipset.as_deref(),
                injection_ok,
                channels,
                &reg,
                &cfg,
                state,
                acquire_state,
                channel_locked,
                valid_pps,
                reacquire_kills,
                rx_zombie_kills,
                rx_silent_seconds,
                video_bps,
            );
            let path = Path::new(crate::paths::WFB_STATS_JSON);
            if let Err(e) = crate::sidecars::write_json_atomic(path, &payload, 0o644) {
                tracing::debug!(error = %e, "ground_wfb_stats_persist_failed");
            }
            // Ship the same body to the logging store as a single full-snapshot
            // event (the durable read source) plus the loss + bitrate samples
            // that round out the link-history series. Best-effort; an absent
            // logging daemon drops these without disturbing receive.
            if let Some(em) = &ingest {
                use ados_protocol::logd::{Fields, Level, Value};
                em.emit_event(
                    "link.wfb_status",
                    Level::Info,
                    json_object_to_fields(&payload),
                );
                let mut tags = Fields::new();
                tags.insert("direction".to_string(), Value::from("uplink"));
                tags.insert("link".to_string(), Value::from("command"));
                em.emit_metric("link.loss_percent", snap.loss_percent, tags.clone());
                em.emit_metric("link.bitrate_kbps", snap.bitrate_kbps as f64, tags);
            }
        }
    }
}

/// Stdout-silence zombie watchdog: terminate the data RX when its per-second
/// stats stream stalls for `RX_HEALTH_SILENCE_THRESHOLD_S` while the process is
/// alive (process-liveness alone is never proof of work). Returns when it kills
/// once or the process exits.
pub async fn zombie_watchdog(
    rx: Arc<DataRxHandle>,
    last_stdout_at: Arc<Mutex<f64>>,
    clock: Arc<dyn Clock>,
    kills: Arc<AtomicU32>,
) {
    use crate::watchdog::RxProcess;
    // Reset the stamp so we don't carry over silence accumulated while the
    // process spawned; give it a full window to start producing stats.
    *last_stdout_at.lock().await = clock.monotonic();
    while rx.is_running() {
        tokio::time::sleep(std::time::Duration::from_secs_f64(
            RX_HEALTH_POLL_INTERVAL_S,
        ))
        .await;
        let silent_for = clock.monotonic() - *last_stdout_at.lock().await;
        if silent_for >= RX_HEALTH_SILENCE_THRESHOLD_S {
            kills.fetch_add(1, Ordering::SeqCst);
            tracing::warn!(
                silent_seconds = silent_for,
                zombie_kills_total = kills.load(Ordering::SeqCst),
                "ground_wfb_rx_zombie_detected"
            );
            rx.terminate();
            *last_stdout_at.lock().await = clock.monotonic();
            return;
        }
    }
}

/// Current ISO-8601 UTC timestamp for the link-stats `timestamp` field.
fn now_iso() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}
