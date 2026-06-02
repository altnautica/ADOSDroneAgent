// The wfb-stats sidecar `json!` literal has many keys; the macro expansion
// needs more headroom than the default recursion limit.
#![recursion_limit = "256"]
//! `ados-radio` binary — the WFB TX service for the drone profile.
//!
//! Mirrors `python -m ados.services.wfb` (drone profile path):
//! waits for the WFB TX key, selects the injection adapter, sets monitor mode,
//! spawns `wfb_tx` in its own process group (setsid + killpg — the structural
//! fix for the orphaned-wfb_tx bug class), runs the Rule-37 watchdogs and the
//! FHSS hop supervisor UDP tasks, writes Contract E sidecars, and shuts down
//! cleanly on SIGTERM/SIGINT.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::sync::Notify;

use ados_radio::adapter;
use ados_radio::bitrate::{
    new_enabled, new_snapshot, BitrateController, BitrateSnapshot, EnabledHandle, SnapshotHandle,
};
use ados_radio::cmdsock::{self, CmdState};
use ados_radio::config::WfbConfig;
use ados_radio::hop::{
    build_hop_announce, build_presence_beacon, derive_pair_key, hop_announce_interval,
    hop_announce_rounds, hop_epoch_ms, parse_hop_ack, parse_presence_beacon, HopState, HopTrigger,
    HOP_ACK_PORT, HOP_CONTROL_PORT, PRESENCE_INTERVAL,
};
use ados_radio::link_quality::LinkStats;
use ados_radio::link_state::derive_link_state;
use ados_radio::paths::{
    read_bind_sentinel_active, run_path, write_sidecar, DRONE_KEY, WFB_TX_KEY,
};
use ados_radio::process::RadioProcesses;
use ados_radio::watchdog::{
    new_counters, tx_health_watchdog, video_recvq_watchdog, CounterHandle, WatchdogCounters,
    WatchdogFired,
};

const CONFIG_YAML: &str = "/etc/ados/config.yaml";
const PROFILE_CONF: &str = "/etc/ados/profile.conf";
/// Poll interval while waiting for the WFB TX key (unpaired state).
const KEY_WAIT_INTERVAL: Duration = Duration::from_secs(5);
/// Peer-beacon freshness window: skip the periodic scan when the peer was heard
/// within this many seconds (the scan locks the radio and drops TX frames).
const PEER_FRESH_SKIP_SECS: f64 = 60.0;
/// `tx_bytes` liveness window: the radio counts as actively injecting RF when
/// its `tx_bytes` counter has moved within this many seconds.
const TX_LIVE_WINDOW: Duration = Duration::from_secs(5);

/// Transmit/uplink rate snapshot surfaced on the heartbeat. `tx_bytes_per_s` is
/// the smoothed radio transmit rate; `valid_rx_packets_per_s` is the uplink
/// valid-decode rate (0 on a drone-only rig with no rx.key, since the stats RX
/// never runs and the drone is the video source, not a receiver).
#[derive(Clone, Copy, Default)]
struct TxRates {
    tx_bytes_per_s: f64,
    valid_rx_packets_per_s: f64,
}

/// Tracks `/sys/class/net/<iface>/statistics/tx_bytes` progress so the heartbeat
/// can report whether RF is actually leaving the antenna AND the smoothed
/// transmit rate. Polled in the 2 s heartbeat loop; `tx_live()` is the "active"
/// signal the link-state derivation uses (the strongest "the radio is injecting"
/// evidence), `tx_bytes_per_s()` is the rate the sidecar surfaces.
struct TxLiveness {
    last_value: u64,
    last_change: Instant,
    seen: bool,
    /// Value + instant of the previous poll, for the rate delta.
    prev_value: u64,
    prev_at: Instant,
    rate_bytes_per_s: f64,
}

impl TxLiveness {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            last_value: 0,
            last_change: now,
            seen: false,
            prev_value: 0,
            prev_at: now,
            rate_bytes_per_s: 0.0,
        }
    }

    /// Feed the current `tx_bytes` counter; records a change instant when it
    /// advances and updates the smoothed transmit rate from the inter-poll
    /// delta. The first reading seeds the baseline without counting as a change.
    fn observe(&mut self, value: u64) {
        let now = Instant::now();
        if !self.seen {
            self.last_value = value;
            self.prev_value = value;
            self.prev_at = now;
            self.seen = true;
            return;
        }
        if value != self.last_value {
            self.last_value = value;
            self.last_change = now;
        }
        // Rate over the elapsed poll interval (counters never decrease, but a
        // wrap/reset is clamped to 0 rather than producing a negative rate).
        let elapsed = now.duration_since(self.prev_at).as_secs_f64();
        if elapsed > 0.0 {
            let delta = value.saturating_sub(self.prev_value) as f64;
            self.rate_bytes_per_s = delta / elapsed;
        }
        self.prev_value = value;
        self.prev_at = now;
    }

    /// True when the counter is non-zero and advanced within the live window.
    fn tx_live(&self) -> bool {
        self.last_value > 0 && self.last_change.elapsed() < TX_LIVE_WINDOW
    }

    /// The smoothed radio transmit rate in bytes/second.
    fn tx_bytes_per_s(&self) -> f64 {
        self.rate_bytes_per_s
    }
}

/// Read `/sys/class/net/<iface>/statistics/tx_bytes`, or `None` when unreadable.
async fn read_tx_bytes(iface: &str) -> Option<u64> {
    let path = format!("/sys/class/net/{}/statistics/tx_bytes", iface);
    tokio::fs::read_to_string(&path)
        .await
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Decide whether a reactive hop should fire from the live link stats.
///
/// `cooldown_allowed` is `HopState::reactive_allowed()` (link established + the
/// 30 s reactive cooldown met). A reactive hop fires only on REAL data: the
/// stats RX must have produced a non-empty timestamp AND a non-zero packet
/// count, because the default `LinkStats` (rssi -100, 0 packets, empty
/// timestamp) would otherwise trip the RSSI threshold and hop every cycle on a
/// drone-only rig that never runs the stats RX (no rx.key). With real data, a
/// hop fires when loss or RSSI crosses its configured threshold.
fn reactive_should_fire(
    cooldown_allowed: bool,
    link: &LinkStats,
    loss_threshold_percent: f64,
    rssi_threshold_dbm: f64,
) -> bool {
    if !cooldown_allowed {
        return false;
    }
    let has_real = !link.timestamp.is_empty() && link.packets_received > 0;
    has_real && (link.loss_percent > loss_threshold_percent || link.rssi_dbm < rssi_threshold_dbm)
}

#[tokio::main]
async fn main() {
    // One-shot adapter-list mode: scan once, print the JSON list to stdout, and
    // exit 0 WITHOUT entering the service loop. This is the seam a thin Python
    // shim and any pre-service caller invokes when no radio service is running
    // (e.g. the bind iface setup or the REST adapter endpoint on a fresh box).
    // No tracing init here — stdout must carry ONLY the JSON document.
    let args: Vec<String> = std::env::args().collect();
    if args
        .iter()
        .skip(1)
        .any(|a| a == "adapters" || a == "--list-adapters")
    {
        run_list_adapters().await;
        return;
    }

    {
        use ados_protocol::logd::layer::LogdLayer;
        use tracing_subscriber::prelude::*;

        // fmt as the primary sink (this binary has no journald layer) plus the
        // logd layer that ships records to the logging daemon's ingest socket;
        // the logd layer is best-effort and never blocks the service. This runs
        // only on the service path, after the one-shot adapter-list mode has
        // already returned (that mode must keep stdout pure JSON).
        let filter =
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
        let _ = tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer())
            .with(LogdLayer::new("ados-radio"))
            .try_init();
    }
    tracing::info!("wfb_service_starting");

    let mut cfg = WfbConfig::load_from(Path::new(CONFIG_YAML));
    // Fold the operator-facing link preset onto the MCS/FEC trio before the
    // radio comes up so the data plane spawns at the preset's tunables. The
    // default `conservative` is a no-op (the explicit config stands).
    cfg.apply_link_preset();
    let cfg = cfg;

    // Profile gate: the WFB TX service is drone-only. On a ground station this
    // binary must idle (the GS runs ados-wfb-rx) so it doesn't clobber the GS's
    // wfb-stats.json. Defensive — the supervisor already profile-gates the unit.
    if ados_radio::config::profile_is_ground_station(
        Path::new(CONFIG_YAML),
        Path::new(PROFILE_CONF),
    ) {
        tracing::warn!("wfb_tx_idle_on_ground_station_profile");
        wait_for_shutdown().await;
        return;
    }

    let cancel = Arc::new(Notify::new());

    // ── Signal handler ────────────────────────────────────────────────────
    {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            wait_for_shutdown().await;
            cancel.notify_waiters();
        });
    }

    run_service(&cfg, cancel).await;
    tracing::info!("wfb_service_stopped");
}

/// One-shot adapter list: scan, print the JSON array to stdout, exit. The
/// document is the same `WifiAdapterInfo` list the service writes to the
/// adapters sidecar, so a caller gets identical data whether it reads the file
/// or invokes this mode.
async fn run_list_adapters() {
    let adapters = adapter::detect_wfb_adapters().await;
    match serde_json::to_string(&adapters) {
        Ok(s) => println!("{s}"),
        // Serialization of a Vec<WifiAdapterInfo> cannot fail in practice; emit
        // an empty array rather than nothing so the caller always parses JSON.
        Err(_) => println!("[]"),
    }
}

/// Write the full detected-adapter list to `/run/ados/wfb-adapters.json`
/// (Contract: the seam permanent-Python + the GCS panel read). Atomic
/// tmp+rename via `write_sidecar`.
fn write_adapters_sidecar(adapters: &[adapter::WifiAdapterInfo]) {
    let v = serde_json::to_value(adapters).unwrap_or_else(|_| serde_json::Value::Array(vec![]));
    let _ = write_sidecar(&run_path("wfb-adapters.json"), &v);
}

/// The wfb-stats `state` string surfaced while the regulatory gate is blocking.
/// The radio is up but refuses to bring up monitor mode / set a channel until the
/// wanted domain verifies, so it parks here with bounded retry rather than
/// radiating on a band the active domain forbids. Distinct from `no_adapter` /
/// `unpaired` so the panel shows the regulatory conflict in one glance.
const STATE_REG_BLOCKED: &str = "reg_blocked";

/// Backoff (seconds) between regulatory-gate retries while blocked. Bounded and
/// short so a transient domain glitch self-heals quickly, but slow enough not to
/// spin `iw reg set` in a tight loop.
const REG_BLOCKED_RETRY_SECS: u64 = 10;

/// A pure decision over a regulatory-gate `Result` plus the strict-mode flag.
/// Extracted so the gate's branching (proceed / block / proceed-anyway under the
/// escape hatch) is unit-testable without standing up `iw`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RegGateDecision {
    /// The gate passed; continue the bring-up.
    Proceed,
    /// The gate failed and strict mode is on; park in `reg_blocked` and retry.
    /// Carries the bland reason code for the log + sidecar.
    Block { reason: &'static str },
    /// The gate failed but strict mode is off (the lab escape hatch); proceed on
    /// a best-effort basis. Carries the reason for the warning log.
    ProceedBestEffort { reason: &'static str },
}

/// Map a gate `Result` + the strict flag to a [`RegGateDecision`]. Pure.
fn decide_reg_gate(result: &Result<(), adapter::RegError>, strict: bool) -> RegGateDecision {
    match result {
        Ok(()) => RegGateDecision::Proceed,
        Err(e) => {
            if strict {
                RegGateDecision::Block {
                    reason: e.reason_code(),
                }
            } else {
                RegGateDecision::ProceedBestEffort {
                    reason: e.reason_code(),
                }
            }
        }
    }
}

async fn run_service(cfg: &WfbConfig, cancel: Arc<Notify>) {
    // Count of radio-group respawns since service start, shared with the
    // heartbeat that surfaces it in the sidecar.
    let restart_count = Arc::new(AtomicU64::new(0));
    // Adaptive bitrate / FEC controller snapshot, shared across respawns so the
    // heartbeat always surfaces the controller's intent (link preset, enable
    // flag, recommended bitrate) even before / between radio bring-ups.
    let bitrate_snapshot: SnapshotHandle = new_snapshot(cfg);
    // Runtime-flippable adaptive-controller enable flag, shared across respawns
    // and between the bitrate controller and the operator command socket so the
    // auto/manual link-tier toggle survives a watchdog kill or a channel hop.
    let adaptive_enabled: EnabledHandle = new_enabled(cfg);
    loop {
        // ── Key guard — block while unpaired ─────────────────────────────
        if !Path::new(WFB_TX_KEY).exists() {
            tracing::info!(key = WFB_TX_KEY, "wfb_blocked_unpaired");
            write_stats_sidecar(
                "unpaired",
                &ChannelTruth::configured(cfg.rendezvous_channel()),
                &RegSnapshot::default(),
                cfg.tx_power_dbm,
                None,
                &LinkStats::default(),
                cfg,
                restart_count.load(Ordering::Relaxed),
                &WatchdogCounters::default(),
                &TxRates::default(),
                &bitrate_snapshot.lock().await.clone(),
            );
            tokio::select! {
                _ = tokio::time::sleep(KEY_WAIT_INTERVAL) => continue,
                _ = cancel.notified() => return,
            }
        }

        // ── Regulatory gate, stage 1: set + VERIFY the domain BEFORE the
        // adapter is brought up in monitor mode. The kernel maps the permitted
        // channel set and the per-channel TX-power ceiling at monitor-mode
        // bring-up, so a domain set afterwards is too late and leaves the home
        // channel (149, 5745 MHz) capped. This is a global per-phy call, so it
        // needs no interface and cannot disturb the operator's management link.
        // On verify failure under the strict (default) gate the radio parks in
        // `reg_blocked` and retries rather than radiating on a forbidden band;
        // the lab escape hatch (`reg_gate_strict: false`) proceeds best-effort.
        // A None config value falls back to the safe default.
        let rendezvous_ch = cfg.rendezvous_channel();
        {
            let domain = cfg.reg_domain.as_deref().unwrap_or("US");
            let reg_result = ados_radio::adapter::set_reg_domain(domain).await;
            match decide_reg_gate(&reg_result, cfg.reg_gate_strict) {
                RegGateDecision::Proceed => {}
                RegGateDecision::ProceedBestEffort { reason } => {
                    tracing::warn!(domain, reason, "wfb_reg_gate_proceeding_best_effort");
                }
                RegGateDecision::Block { reason } => {
                    tracing::error!(domain, reason, "wfb_reg_gate_blocked");
                    // Surface the live domain vs the wanted one so the panel shows
                    // the actual conflict (e.g. a baked country the global set
                    // could not displace), not a configured-and-locked lie.
                    let status = ados_radio::adapter::read_reg_status(domain).await;
                    write_stats_sidecar(
                        STATE_REG_BLOCKED,
                        &ChannelTruth::configured(rendezvous_ch),
                        &RegSnapshot {
                            domain: status.domain,
                            verified: status.verified,
                            enabled_channels: Vec::new(),
                        },
                        cfg.tx_power_dbm,
                        None,
                        &LinkStats::default(),
                        cfg,
                        restart_count.load(Ordering::Relaxed),
                        &WatchdogCounters::default(),
                        &TxRates::default(),
                        &bitrate_snapshot.lock().await.clone(),
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(REG_BLOCKED_RETRY_SECS)) => continue,
                        _ = cancel.notified() => return,
                    }
                }
            }
        }

        // ── Adapter selection ─────────────────────────────────────────────
        // Detect every adapter (the full list feeds the adapters sidecar) and
        // pick the verified injection radio. The outcome carries the scan
        // counts for the loud no-injection diagnostic.
        let outcome = adapter::detect_and_select(&cfg.interface).await;
        // Publish the full detected list so the GCS panel + the permanent-Python
        // seam see the scan verdict regardless of whether a radio was found.
        write_adapters_sidecar(&outcome.adapters);
        let Some(adapter) = outcome.selected.clone() else {
            // No RTL injection-capable adapter could be proven. Fail LOUDLY with
            // the diagnostic counts (total detected + compatible-and-monitor),
            // and keep the sidecar's stranded-radio signal — adapter chipset
            // null, adapter_injection_ok false — so the panel shows the warning
            // rather than a false "connecting" with zero injected frames.
            tracing::error!(
                total_adapters = outcome.total(),
                compatible = outcome.compatible_monitor(),
                note = "no RTL injection-capable adapter verified; not starting TX",
                "wfb_no_injection_adapter"
            );
            write_stats_sidecar(
                "no_adapter",
                &ChannelTruth::configured(rendezvous_ch),
                &RegSnapshot::default(),
                cfg.tx_power_dbm,
                None, // None adapter → chipset "" + adapter_injection_ok false
                &LinkStats::default(),
                cfg,
                restart_count.load(Ordering::Relaxed),
                &WatchdogCounters::default(),
                &TxRates::default(),
                &bitrate_snapshot.lock().await.clone(),
            );
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(10)) => continue,
                _ = cancel.notified() => return,
            }
        };

        tracing::info!(
            iface = %adapter.ifname,
            chipset = %adapter.chipset,
            injection_ok = adapter.injection_ok,
            "adapter_selected"
        );

        // ── Regulatory gate, stage 2: assert the rendezvous channel is in the
        // domain's enabled set and is non-DFS, NOW that the interface (and its
        // wiphy) exist, BEFORE setting the channel / TX power / spawning wfb_tx.
        // This catches a domain/channel mismatch at preflight instead of as a
        // silent power-cap on a fallback frequency. An empty enabled set means
        // the wiphy list was unreadable, which the assertion treats as "could
        // not determine" and passes, so a board with an unreadable channel list
        // still comes up. On a strict-mode failure the radio parks in
        // `reg_blocked` and retries; no wfb_tx is ever spawned on a bad channel.
        let iface = &adapter.ifname;
        let gate_enabled: std::collections::BTreeSet<u8> = adapter::enabled_channels(iface).await;
        let gate_dfs: std::collections::BTreeSet<u8> = adapter::dfs_channels(iface).await;
        {
            let ready =
                adapter::assert_reg_ready(rendezvous_ch, &gate_enabled, &gate_dfs, cfg.dfs_allowed);
            match decide_reg_gate(&ready, cfg.reg_gate_strict) {
                RegGateDecision::Proceed => {}
                RegGateDecision::ProceedBestEffort { reason } => {
                    tracing::warn!(
                        channel = rendezvous_ch,
                        reason,
                        "wfb_reg_gate_channel_proceeding_best_effort"
                    );
                }
                RegGateDecision::Block { reason } => {
                    tracing::error!(
                        channel = rendezvous_ch,
                        reason,
                        "wfb_reg_gate_channel_blocked"
                    );
                    // The wiphy exists now, so surface the live domain + the
                    // permitted set: the panel can show that the rendezvous home
                    // is not in the enabled set under this domain.
                    let domain = cfg.reg_domain.as_deref().unwrap_or("US");
                    let status = ados_radio::adapter::read_reg_status(domain).await;
                    write_stats_sidecar(
                        STATE_REG_BLOCKED,
                        &ChannelTruth::configured(rendezvous_ch),
                        &RegSnapshot {
                            domain: status.domain,
                            verified: status.verified,
                            enabled_channels: gate_enabled.iter().copied().collect(),
                        },
                        cfg.tx_power_dbm,
                        None,
                        &LinkStats::default(),
                        cfg,
                        restart_count.load(Ordering::Relaxed),
                        &WatchdogCounters::default(),
                        &TxRates::default(),
                        &bitrate_snapshot.lock().await.clone(),
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(REG_BLOCKED_RETRY_SECS)) => continue,
                        _ = cancel.notified() => return,
                    }
                }
            }
        }
        set_channel(iface, cfg.channel).await;

        // ── Clamp TX power BEFORE wfb_tx starts injecting ─────────────────
        // Critical on host-VBUS rigs: the driver default (~17-20 dBm) browns
        // out the adapter. Ramps up from the configured floor on rejection.
        let effective_tx_dbm = ados_radio::adapter::set_tx_power(iface, cfg.tx_power_dbm)
            .await
            .unwrap_or(cfg.tx_power_dbm);

        // ── Load pair key for HMAC derivation ────────────────────────────
        let drone_key = tokio::fs::read(DRONE_KEY).await.ok();
        let pair_key = derive_pair_key(drone_key.as_deref());

        // ── Regulatory-enabled channel set, for the hop target filter ─────
        // Channels this adapter's reg domain forbids fail `iw set channel` with
        // -22 and split the pair onto divergent frequencies; the hop loop
        // intersects its candidates with this set. Empty = "could not
        // determine" → do not restrict. Reuse the set the regulatory gate
        // already read for this adapter bring-up (no second `iw` call).
        let enabled_channels = Arc::new(gate_enabled);
        // The permitted-set Vec the heartbeat surfaces in every sidecar (the
        // ordered enabled channels). Cloned once here, reused each tick.
        let enabled_channels_vec: Vec<u8> = enabled_channels.iter().copied().collect();

        // ── Received-side link proof (the drone's received-side signal) ───
        // A transmit-only end has no decode stats of its own, so `channel_locked`
        // and `rf_unverified` are derived from whether a verified return signal
        // (a control-plane ack or a peer beacon) was heard recently. The control-
        // plane listener records proof; the heartbeat reads it. The monotonic
        // `reference` is the shared origin all observations are measured against.
        let rx_proof = ados_radio::link_proof::RxProof::new();
        let proof_reference = Instant::now();
        // Runtime operating channel (tmpfs concept): equals the rendezvous home
        // until a coordinated channel move commits. The hop supervisor updates it
        // on a successful channel change; the heartbeat reads it for the
        // `operating_channel` field. Seeded with the rendezvous home.
        let operating_channel = Arc::new(AtomicU64::new(rendezvous_ch as u64));
        // The wanted regulatory domain, resolved once (constant for this bring-up)
        // so the heartbeat's cheap live read can report verified vs unverified.
        let wanted_domain = cfg.reg_domain.clone().unwrap_or_else(|| "US".to_string());

        // ── Shared live link stats (fed by the stats-RX reader task) ──────
        let link = Arc::new(tokio::sync::Mutex::new(LinkStats::default()));
        // ── Shared watchdog counters (zombie/video-stall kills) — the same ─
        // share pattern as the link stats; the heartbeat reads these onto the
        // sidecar, the watchdogs update them on fire.
        let counters: CounterHandle = new_counters();

        // ── Spawn the radio process group: data wfb_tx + tx/rx control + ──
        // stats rx (each in its own session — the orphan fix; control plane
        // carries HopAnnounce/HopAck over the air, so it MUST run for FHSS).
        let key_path = Path::new(WFB_TX_KEY);
        let proc = match RadioProcesses::spawn(iface, cfg, key_path, link.clone()).await {
            Ok(p) => Arc::new(tokio::sync::Mutex::new(p)),
            Err(e) => {
                tracing::warn!(error = %e, "wfb_spawn_failed");
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(5)) => continue,
                    _ = cancel.notified() => return,
                }
            }
        };
        let pid = { proc.lock().await.data_tx_pid().unwrap_or(0) };
        let adapter_info = AdapterInfo {
            interface: iface.clone(),
            chipset: adapter.chipset.clone(),
            injection_ok: adapter.injection_ok,
            usb_speed_mbps: adapter.usb_speed_mbps,
            usb_degraded: adapter.usb_degraded,
        };
        // Live regulatory status for the sidecar (the wanted domain is constant
        // for this bring-up; the live read is a cheap `iw reg get`). Re-read by
        // the heartbeat each tick so a domain that changes under the radio
        // surfaces, but seeded here so the first `connecting` sidecar is truthful.
        let reg_status = ados_radio::adapter::read_reg_status(&wanted_domain).await;
        let reg_snapshot = RegSnapshot {
            domain: reg_status.domain,
            verified: reg_status.verified,
            enabled_channels: enabled_channels_vec.clone(),
        };
        write_stats_sidecar(
            "connecting",
            &ChannelTruth::configured(rendezvous_ch),
            &reg_snapshot,
            effective_tx_dbm,
            Some(&adapter_info),
            &LinkStats::default(),
            cfg,
            restart_count.load(Ordering::Relaxed),
            &WatchdogCounters::default(),
            &TxRates::default(),
            &bitrate_snapshot.lock().await.clone(),
        );
        tracing::info!(iface, channel = cfg.channel, pid, "wfb_service_ready");

        // ── Run watchdogs + hop supervisor concurrently ──────────────────
        let task_cancel = cancel.clone();
        let iface_str = iface.clone();

        // 2 s sidecar heartbeat — reads the live link stats + the tx_bytes
        // liveness + the bind sentinel, derives the link state, and keeps
        // wfb-stats.json fresh so the REST handler never marks it stale
        // (mtime > 10 s).
        let hb_cancel = task_cancel.clone();
        let hb_cfg = cfg.clone();
        let hb_adapter = adapter_info.clone();
        let hb_rendezvous = rendezvous_ch;
        let hb_link = link.clone();
        let hb_iface = iface_str.clone();
        let hb_restart = restart_count.clone();
        let hb_counters = counters.clone();
        let hb_bitrate = bitrate_snapshot.clone();
        let hb_proof = rx_proof.clone();
        let hb_proof_reference = proof_reference;
        let hb_operating = operating_channel.clone();
        let hb_wanted_domain = wanted_domain.clone();
        let hb_enabled = enabled_channels_vec.clone();
        let mut heartbeat = tokio::spawn(async move {
            const HEARTBEAT_INTERVAL_S: f64 = 2.0;
            let mut tick = tokio::time::interval(Duration::from_secs(2));
            let mut tx_live = TxLiveness::new();
            let mut prev_packets: i64 = 0;
            // Last successfully-read live channel; if `iw info` momentarily fails
            // we keep reporting the last-known live value rather than the
            // configured one (a transient read error is not a channel change).
            let mut last_live_channel = hb_rendezvous;
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        if let Some(v) = read_tx_bytes(&hb_iface).await {
                            tx_live.observe(v);
                        }
                        let stats = hb_link.lock().await.clone();
                        let wd = *hb_counters.lock().await;
                        // Uplink valid-decode rate over the heartbeat interval.
                        // 0 on a drone-only rig (no rx.key → packets stay 0).
                        let pkt_delta = (stats.packets_received - prev_packets).max(0) as f64;
                        prev_packets = stats.packets_received;
                        let rates = TxRates {
                            tx_bytes_per_s: tx_live.tx_bytes_per_s(),
                            valid_rx_packets_per_s: pkt_delta / HEARTBEAT_INTERVAL_S,
                        };
                        // The key can be removed (unpair) at runtime; re-check.
                        let tx_key_present = Path::new(WFB_TX_KEY).exists();
                        let bind_active = read_bind_sentinel_active();
                        let state = derive_link_state(
                            tx_key_present,
                            bind_active,
                            &stats,
                            tx_live.tx_live(),
                        );
                        let bsnap = hb_bitrate.lock().await.clone();

                        // Truthful channel: read the LIVE interface channel, not
                        // the configured value. A momentary read failure keeps the
                        // last-known live value.
                        if let Some(live) = channel_from_iface(&hb_iface).await {
                            last_live_channel = live;
                        }
                        // Received-side proof: the drone is locked only when a
                        // verified return signal was heard within the grace window;
                        // it is `rf_unverified` when injecting RF with no such
                        // signal (the transmitting-zero-reception case).
                        let now = Instant::now();
                        let rx_proven = hb_proof.proven_within(
                            ados_radio::link_proof::RX_PROOF_GRACE,
                            now,
                            hb_proof_reference,
                        );
                        let channels = ChannelTruth {
                            actual: last_live_channel,
                            rendezvous: hb_rendezvous,
                            operating: hb_operating.load(Ordering::Relaxed) as u8,
                            locked: rx_proven,
                            rf_unverified: ados_radio::link_proof::is_rf_unverified(
                                tx_live.tx_live(),
                                rx_proven,
                            ),
                        };
                        // Live regulatory status (cheap `iw reg get`), so a domain
                        // that changes under the radio surfaces remotely too.
                        let reg_status =
                            ados_radio::adapter::read_reg_status(&hb_wanted_domain).await;
                        let reg = RegSnapshot {
                            domain: reg_status.domain,
                            verified: reg_status.verified,
                            enabled_channels: hb_enabled.clone(),
                        };

                        write_stats_sidecar(
                            state.as_str(),
                            &channels,
                            &reg,
                            effective_tx_dbm,
                            Some(&hb_adapter),
                            &stats,
                            &hb_cfg,
                            hb_restart.load(Ordering::Relaxed),
                            &wd,
                            &rates,
                            &bsnap,
                        );
                    }
                    _ = hb_cancel.notified() => break,
                }
            }
        });

        let tx_cancel = task_cancel.clone();
        let tx_iface = iface_str.clone();
        let tx_counters = counters.clone();
        let mut watchdog1 = tokio::spawn(async move {
            tx_health_watchdog(&tx_iface, pid, tx_counters, tx_cancel).await
        });

        let recvq_cancel = task_cancel.clone();
        let recvq_counters = counters.clone();
        let mut watchdog2 =
            tokio::spawn(async move { video_recvq_watchdog(recvq_counters, recvq_cancel).await });

        // Data-tx exit watch. The counter watchdog only fires after a 30 s flat
        // window, so a `wfb_tx` that crashes on its own (segfault, OOM kill, a
        // driver-rejected arg on respawn) would otherwise leave the link dead for
        // up to 30 s. This arm polls the data-plane child's liveness on a 1 s
        // cadence and completes the moment it has exited, tripping an immediate
        // respawn of the whole radio group via the run-loop select. A brief lock
        // per poll (a non-blocking `try_wait` reap) does not contend with the FEC/
        // MCS setters in practice.
        let exit_cancel = task_cancel.clone();
        let exit_proc = proc.clone();
        let mut data_tx_exit = tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.tick().await; // consume the immediate first tick
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        if !exit_proc.lock().await.data_tx_running() {
                            tracing::warn!("wfb_data_tx_exited_respawning");
                            return;
                        }
                    }
                    _ = exit_cancel.notified() => return,
                }
            }
        });

        // Adaptive bitrate / FEC controller. Off by default (it only refreshes
        // the snapshot when disabled); when enabled it restarts only the data
        // plane to apply a new FEC on sustained link degradation. It never ends
        // on its own, so it is not part of the respawn-trigger select arm —
        // it's aborted alongside the other siblings on respawn/shutdown.
        let bc_cancel = task_cancel.clone();
        let bc_link = link.clone();
        let bc_proc = proc.clone();
        let bc_snapshot = bitrate_snapshot.clone();
        let bc_enabled = adaptive_enabled.clone();
        let bitrate_ctrl = tokio::spawn(async move {
            BitrateController::new(bc_enabled)
                .run(bc_link, bc_proc, bc_snapshot, bc_cancel)
                .await;
        });

        // Operator command socket: serves the live FEC/MCS/TX-power/tier knobs to
        // the REST layer when the native radio is the running transmit plane.
        // Holds the SAME process handle + adaptive flag the controller uses, so a
        // knob change reaches the live radio. Spawned per bring-up alongside the
        // sibling tasks (it is aborted + re-served with the new process handle on
        // every respawn, the same lifecycle as the heartbeat + watchdogs).
        let cmd_state = CmdState {
            proc: proc.clone(),
            adaptive_enabled: adaptive_enabled.clone(),
        };
        let cmd_cancel = task_cancel.clone();
        let cmd_sock_path = ados_radio::paths::run_path("wfb-cmd.sock");
        let cmd_server = tokio::spawn(async move {
            tokio::select! {
                r = cmdsock::serve(cmd_state, Path::new(&cmd_sock_path)) => {
                    if let Err(e) = r {
                        tracing::warn!(error = %e, "wfb_command_socket_serve_ended");
                    }
                }
                _ = cmd_cancel.notified() => {}
            }
        });

        let hop_cancel = task_cancel.clone();
        let hop_iface = iface_str.clone();
        let hop_proc = proc.clone();
        let hop_cfg = cfg.clone();
        let hop_key = pair_key;
        let presence_cancel = task_cancel.clone();
        let device_id = read_device_id();

        // Presence beacon emitter (10s interval). It advertises the LIVE channel
        // (read from the interface each tick), not the configured value, so a GS
        // that hears the beacon jumps to where the drone ACTUALLY is. A beacon
        // that lies about its channel is the loop that hides a fallback-frequency
        // landing; feeding the live value turns it into a true rendezvous pointer.
        let beacon_cancel = presence_cancel.clone();
        let beacon_key = hop_key;
        let beacon_iface = iface_str.clone();
        let beacon_fallback = rendezvous_ch;
        let beacon_device = device_id.clone();
        let mut beacon = tokio::spawn(async move {
            emit_presence_beacons(
                &beacon_device,
                &beacon_iface,
                beacon_fallback,
                &beacon_key,
                beacon_cancel,
            )
            .await
        });

        // Hop supervisor (enabled only when configured). When hop is disabled the
        // else branch still runs an always-on control-plane proof listener so the
        // drone's received-side proof (`channel_locked` / `rf_unverified`) works
        // regardless of hop config — both paths feed the same shared `RxProof`.
        let hop_enabled = hop_cfg.auto_hop_enabled;
        let hop_link = link.clone();
        let hop_enabled_channels = enabled_channels.clone();
        let hop_restart = restart_count.clone();
        let hop_proof = rx_proof.clone();
        let hop_proof_reference = proof_reference;
        let hop_operating = operating_channel.clone();
        let mut hop = tokio::spawn(async move {
            if hop_enabled {
                run_hop_supervisor(
                    &hop_iface,
                    &hop_cfg,
                    hop_proc,
                    &hop_key,
                    &device_id,
                    hop_link,
                    hop_enabled_channels,
                    hop_restart,
                    hop_proof,
                    hop_proof_reference,
                    hop_operating,
                    hop_cancel,
                )
                .await;
            } else {
                proof_only_listener(
                    &hop_key,
                    &device_id,
                    hop_proof,
                    hop_proof_reference,
                    hop_cancel,
                )
                .await;
            }
        });

        // Wait for any task to finish (cancel → shutdown; watchdog → respawn).
        // `&mut` the handles so the un-selected ones are NOT dropped-and-detached
        // here — we abort them explicitly below so tasks don't pile up across
        // respawns.
        tokio::select! {
            result = &mut watchdog1 => {
                if let Ok(WatchdogFired::TxStalled | WatchdogFired::RecvqBacklog) = result {
                    tracing::warn!("watchdog_fired_killing_wfb_tx");
                }
            }
            result = &mut watchdog2 => {
                if let Ok(WatchdogFired::RecvqBacklog) = result {
                    tracing::warn!("video_recvq_watchdog_fired");
                }
            }
            _ = &mut data_tx_exit => {
                // The data-plane wfb_tx exited on its own — respawn the whole
                // group immediately (the falls-through path below handles it).
            }
            _ = &mut hop => {}
            _ = &mut beacon => {}
            _ = &mut heartbeat => {}
            _ = cancel.notified() => {
                // Clean shutdown: stop the tasks, the radio group, then restore
                // the adapter to managed mode so it isn't left stuck in monitor.
                heartbeat.abort();
                watchdog1.abort();
                watchdog2.abort();
                data_tx_exit.abort();
                bitrate_ctrl.abort();
                cmd_server.abort();
                hop.abort();
                beacon.abort();
                proc.lock().await.kill_all().await;
                ados_radio::adapter::set_managed_mode(iface).await;
                tracing::info!("wfb_service_stopping");
                return;
            }
        }

        // A task exited (watchdog fired / hop ended / data-tx self-crashed) —
        // abort the siblings so they don't accumulate, kill the whole radio
        // group, and respawn.
        heartbeat.abort();
        watchdog1.abort();
        watchdog2.abort();
        data_tx_exit.abort();
        bitrate_ctrl.abort();
        cmd_server.abort();
        hop.abort();
        beacon.abort();
        proc.lock().await.kill_all().await;
        // The radio group will respawn at the top of the loop — count it.
        restart_count.fetch_add(1, Ordering::Relaxed);
        let wd = *counters.lock().await;
        // Re-read the live regulatory status for the respawn-transient sidecar so
        // it carries the truth (domain + verified) rather than a stale snapshot.
        let respawn_reg_status = ados_radio::adapter::read_reg_status(&wanted_domain).await;
        write_stats_sidecar(
            "connecting",
            &ChannelTruth::configured(rendezvous_ch),
            &RegSnapshot {
                domain: respawn_reg_status.domain,
                verified: respawn_reg_status.verified,
                enabled_channels: enabled_channels_vec.clone(),
            },
            effective_tx_dbm,
            Some(&adapter_info),
            &LinkStats::default(),
            cfg,
            restart_count.load(Ordering::Relaxed),
            &wd,
            &TxRates::default(),
            &bitrate_snapshot.lock().await.clone(),
        );
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            _ = cancel.notified() => return,
        }
    }
}

/// Emit PresenceBeacons on UDP 127.0.0.1:5803 every 10s, advertising the LIVE
/// interface channel so the beacon points the peer at where this rig actually
/// is. `fallback_channel` is used only when the live channel cannot be read this
/// tick (a transient `iw info` failure), so the beacon never advertises a stale
/// or configured value when the live one is available.
async fn emit_presence_beacons(
    device_id: &str,
    iface: &str,
    fallback_channel: u8,
    pair_key: &[u8; 32],
    cancel: Arc<Notify>,
) {
    let Ok(sock) = tokio::net::UdpSocket::bind("0.0.0.0:0").await else {
        return;
    };
    let mut tick = tokio::time::interval(PRESENCE_INTERVAL);
    let mut last_live = fallback_channel;
    loop {
        tokio::select! {
            _ = tick.tick() => {
                if let Some(live) = channel_from_iface(iface).await {
                    last_live = live;
                }
                let epoch = hop_epoch_ms();
                let pkt = build_presence_beacon(
                    device_id,
                    true, // drone role
                    last_live,
                    0,    // rssi not known at drone-side TX
                    epoch,
                    pair_key,
                );
                let _ = sock
                    .send_to(&pkt, format!("127.0.0.1:{HOP_CONTROL_PORT}"))
                    .await;
            }
            _ = cancel.notified() => return,
        }
    }
}

/// An always-on control-plane proof listener for when the hop supervisor is
/// disabled (`auto_hop_enabled: false`). The hop supervisor owns the 5810
/// listener when enabled; when it is not, this minimal listener binds the same
/// port and records a verified return signal (a HopAck or a peer PresenceBeacon)
/// into the shared `RxProof`, so the drone's received-side lock proof and the
/// `rf_unverified` flag work regardless of hop config. It only updates the proof
/// — it never moves a channel. The own-device-id check drops a loopback copy of
/// this rig's own beacon so a self-beacon never counts as a return signal.
async fn proof_only_listener(
    pair_key: &[u8; 32],
    device_id: &str,
    rx_proof: ados_radio::link_proof::RxProof,
    reference: Instant,
    cancel: Arc<Notify>,
) {
    let sock = match tokio::net::UdpSocket::bind(format!("0.0.0.0:{HOP_ACK_PORT}")).await {
        Ok(s) => s,
        Err(e) => {
            // The port is taken or unbindable: fall back to a plain wait so the
            // task still ends cleanly on cancel rather than spinning.
            tracing::warn!(error = %e, "proof_listener_bind_failed");
            cancel.notified().await;
            return;
        }
    };
    let pair_key = *pair_key;
    let own_device_id = device_id.to_string();
    let mut buf = [0u8; 128];
    loop {
        tokio::select! {
            r = sock.recv_from(&mut buf) => {
                let Ok((n, _)) = r else { continue };
                let pkt = &buf[..n];
                if parse_hop_ack(pkt, &pair_key).is_some() {
                    rx_proof.observe(Instant::now(), reference);
                } else if let Some(p) = parse_presence_beacon(pkt, &pair_key) {
                    if !is_self_beacon(&own_device_id, &p.device_id) {
                        rx_proof.observe(Instant::now(), reference);
                    }
                }
            }
            _ = cancel.notified() => return,
        }
    }
}

/// FHSS hop supervisor. A dedicated 5810 listener decodes the control plane
/// (HopAck + the peer's PresenceBeacon) and drives the shared `HopState`; the
/// hop loop announces a target, waits for the matching ACK, then executes the
/// channel change. Writes `hop-supervisor.json` (5 s) + `peer-presence.json`.
#[allow(clippy::too_many_arguments)]
async fn run_hop_supervisor(
    iface: &str,
    cfg: &WfbConfig,
    proc: Arc<tokio::sync::Mutex<RadioProcesses>>,
    pair_key: &[u8; 32],
    device_id: &str,
    link: Arc<tokio::sync::Mutex<LinkStats>>,
    enabled_channels: Arc<std::collections::BTreeSet<u8>>,
    restart_count: Arc<AtomicU64>,
    rx_proof: ados_radio::link_proof::RxProof,
    proof_reference: Instant,
    operating_channel: Arc<AtomicU64>,
    cancel: Arc<Notify>,
) {
    let state = Arc::new(tokio::sync::Mutex::new(HopState::new(cfg.channel)));
    let pair_key = *pair_key; // [u8;32] is Copy — move into tasks freely.
                              // None when the adapter's reg domain could not be read (do not restrict);
                              // Some(&set) drives the hop-target intersection + the sidecar field.
    let enabled_opt: Option<&std::collections::BTreeSet<u8>> =
        (!enabled_channels.is_empty()).then(|| enabled_channels.as_ref());

    // ── Control-plane listener on 5810: HopAck vs PresenceBeacon ──────────
    let ack_sock = match tokio::net::UdpSocket::bind(format!("0.0.0.0:{HOP_ACK_PORT}")).await {
        Ok(s) => Arc::new(s),
        Err(e) => {
            tracing::warn!(error = %e, "hop_ack_socket_bind_failed");
            cancel.notified().await;
            return;
        }
    };
    // Acked target channels flow from the listener to the hop loop.
    let (ack_tx, mut ack_rx) = tokio::sync::mpsc::channel::<u8>(8);
    let lst_state = state.clone();
    let lst_cancel = cancel.clone();
    let lst_sock = ack_sock.clone();
    // Own device-id, read once for the self-beacon drop. The beacon carries a
    // 16-byte device-id, so the loopback-delivered copy of this rig's own beacon
    // is recognised by its first 16 bytes.
    let own_device_id = device_id.to_string();
    // The shared received-side proof: a verified HopAck or a peer PresenceBeacon
    // is the drone's evidence that its energy reached a receiver. The heartbeat
    // reads this to compute `channel_locked` / `rf_unverified`.
    let lst_proof = rx_proof.clone();
    let listener = tokio::spawn(async move {
        let mut buf = [0u8; 128];
        // Tracks the last peer device-id back-filled so the cross-process write
        // only fires on a CHANGE (matches the Python `previous != device_id`).
        let mut last_backfilled: Option<String> = None;
        loop {
            tokio::select! {
                r = lst_sock.recv_from(&mut buf) => {
                    let Ok((n, _)) = r else { continue };
                    let pkt = &buf[..n];
                    if let Some(target) = parse_hop_ack(pkt, &pair_key) {
                        // A verified ack from the peer is received-side proof.
                        lst_proof.observe(Instant::now(), proof_reference);
                        let _ = ack_tx.try_send(target);
                    } else if let Some(p) = parse_presence_beacon(pkt, &pair_key) {
                        // Drop this rig's own beacon (a loopback race can deliver
                        // it) so a self-beacon never registers as a peer.
                        if is_self_beacon(&own_device_id, &p.device_id) {
                            continue;
                        }
                        // A verified peer beacon is received-side proof too.
                        lst_proof.observe(Instant::now(), proof_reference);
                        let peer_id = p.device_id.clone();
                        lst_state.lock().await.on_peer_beacon(p);
                        write_peer_presence_json(&lst_state).await;
                        // On a NEW peer device-id, signal the back-fill so the
                        // persisted pair state learns the peer over the radio
                        // (the bind tunnel does not always carry it). The signal
                        // is a small additive sidecar a Python REST consumer
                        // reads to call update_peer_device_id — Rust does not
                        // round-trip config.yaml itself, which would risk
                        // clobbering unrelated keys.
                        if last_backfilled.as_deref() != Some(peer_id.as_str()) {
                            write_peer_backfill_json(&peer_id);
                            last_backfilled = Some(peer_id);
                        }
                    }
                }
                _ = lst_cancel.notified() => break,
            }
        }
    });

    // ── hop-supervisor.json writer (5 s) ──────────────────────────────────
    let hb_state = state.clone();
    let hb_cancel = cancel.clone();
    let hb_cfg = cfg.clone();
    let hb_enabled = enabled_channels.clone();
    let hb_writer = tokio::spawn(async move {
        let mut t = tokio::time::interval(Duration::from_secs(5));
        loop {
            tokio::select! {
                _ = t.tick() => write_hop_supervisor_json(&hb_state, &hb_cfg, &hb_enabled).await,
                _ = hb_cancel.notified() => break,
            }
        }
    });

    let announce_sock = match tokio::net::UdpSocket::bind("0.0.0.0:0").await {
        Ok(s) => s,
        Err(_) => {
            listener.abort();
            hb_writer.abort();
            cancel.notified().await;
            return;
        }
    };

    // Floor the hop period at 15 s. A periodic hop runs a live channel scan that
    // locks the radio for several seconds and drops wfb_tx frames, so an operator
    // (or a bad config) asking for a very short period would starve the video
    // link; a 0 would also panic `interval`. The floor caps the scan rate while
    // leaving longer configured periods untouched.
    const HOP_PERIOD_FLOOR_SECS: u64 = 15;
    let hop_period_secs = (cfg.hop_period_seconds as u64).max(HOP_PERIOD_FLOOR_SECS);
    let mut hop_tick = tokio::time::interval(Duration::from_secs(hop_period_secs));
    let mut stale_tick = tokio::time::interval(Duration::from_secs(5));

    loop {
        tokio::select! {
            _ = hop_tick.tick() => {
                // Never change channel while a bind owns the adapter: the bind
                // stops the normal wfb unit so its bind profile can own the radio
                // exclusively, and a racing iw-channel + wfb_tx restart would
                // corrupt the bind key exchange. The sentinel is a cheap sync
                // file read, so no socket round-trip on this hot tick.
                if read_bind_sentinel_active() {
                    continue;
                }
                if !state.lock().await.can_hop() {
                    continue;
                }
                // Skip the periodic scan while the peer is fresh (<60 s): the
                // scan locks the radio for several seconds and drops wfb_tx
                // frames, so on a healthy link the rescan is pure waste. A
                // reactive scan (handled in the other arm) always runs.
                if state.lock().await.peer_fresh_within(PEER_FRESH_SKIP_SECS) {
                    continue;
                }
                let cur = state.lock().await.channel;
                // Scan live for the quietest enabled in-band channel (rotates if
                // the scan is flat, e.g. monitor mode rejected it).
                let target =
                    ados_radio::channel::pick_hop_target(iface, cur, &cfg.band, enabled_opt).await;
                // The scan can strand the iface in managed mode on some drivers;
                // re-assert monitor mode + retune regardless of whether a hop
                // follows so wfb_tx keeps injecting.
                ados_radio::adapter::restore_monitor_if_needed(iface, cur).await;
                if target == cur {
                    continue;
                }
                try_execute_hop(
                    iface, cfg, &proc, &state, &announce_sock, &mut ack_rx, &pair_key,
                    target, HopTrigger::Periodic, "periodic", &link, &restart_count,
                )
                .await;
            }
            // Reactive trigger + peer-stale return-to-home (every 5 s).
            _ = stale_tick.tick() => {
                // Suppress all actuation during a bind, same as the periodic arm.
                if read_bind_sentinel_active() {
                    continue;
                }
                // Reactive: the live link crossed a loss/RSSI threshold. Gated on
                // REAL data (timestamp + packets) so default stats never trip it.
                let do_reactive = {
                    let cooldown_allowed = state.lock().await.reactive_allowed();
                    let l = link.lock().await;
                    reactive_should_fire(
                        cooldown_allowed,
                        &l,
                        cfg.hop_loss_threshold_percent as f64,
                        cfg.hop_rssi_threshold_dbm as f64,
                    )
                };
                if do_reactive {
                    let cur = state.lock().await.channel;
                    let target = ados_radio::channel::pick_hop_target(
                        iface, cur, &cfg.band, enabled_opt,
                    )
                    .await;
                    ados_radio::adapter::restore_monitor_if_needed(iface, cur).await;
                    if target != cur {
                        tracing::info!(target, "hop_reactive_trigger");
                        try_execute_hop(
                            iface, cfg, &proc, &state, &announce_sock, &mut ack_rx, &pair_key,
                            target, HopTrigger::Reactive, "reactive", &link, &restart_count,
                        )
                        .await;
                    }
                }

                let (return_home, home) = {
                    let s = state.lock().await;
                    (s.should_return_home(), s.home_channel)
                };
                if return_home {
                    tracing::info!(home, "hop_return_home");
                    proc.lock().await.kill_all().await;
                    // Always attempt the channel set AND the respawn (never leave
                    // the radio group dead), but a silent `iw set channel` failure
                    // makes the recorded outcome false: respawning the radio on
                    // the wrong channel is not a successful return home.
                    let channel_ok = set_channel(iface, home).await;
                    let spawn_ok = match RadioProcesses::spawn(iface, cfg, Path::new(WFB_TX_KEY), link.clone()).await {
                        Ok(new_proc) => {
                            *proc.lock().await = new_proc;
                            restart_count.fetch_add(1, Ordering::Relaxed);
                            true
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "return_home_restart_failed");
                            false
                        }
                    };
                    state.lock().await.record_hop(home, "return_home", channel_ok && spawn_ok);
                }

                // Keep the shared operating channel in sync with the hop state's
                // current channel, so the heartbeat's `operating_channel` field
                // reflects a committed move. It equals the rendezvous home until a
                // hop changes it (and returns to home on the return-home path).
                operating_channel.store(state.lock().await.channel as u64, Ordering::Relaxed);
            }
            _ = cancel.notified() => {
                listener.abort();
                hb_writer.abort();
                return;
            }
        }
    }
}

/// Announce a hop to `target`, wait for the matching ACK, and on success
/// execute the channel change (kill the radio group → `iw set channel` →
/// respawn). Records the outcome in the hop history with `label`. Shared by the
/// periodic and reactive triggers.
#[allow(clippy::too_many_arguments)]
async fn try_execute_hop(
    iface: &str,
    cfg: &WfbConfig,
    proc: &Arc<tokio::sync::Mutex<RadioProcesses>>,
    state: &Arc<tokio::sync::Mutex<HopState>>,
    announce_sock: &tokio::net::UdpSocket,
    ack_rx: &mut tokio::sync::mpsc::Receiver<u8>,
    pair_key: &[u8; 32],
    target: u8,
    trigger: HopTrigger,
    label: &str,
    link: &Arc<tokio::sync::Mutex<LinkStats>>,
    restart_count: &Arc<AtomicU64>,
) {
    let epoch = hop_epoch_ms();
    let pkt = build_hop_announce(epoch, target, trigger, pair_key);
    // Drain stale acks so we only count one for THIS announce.
    while ack_rx.try_recv().is_ok() {}

    // Announce up to 30×@100ms, stop early on the matching ACK.
    let mut acked = false;
    for _ in 0..hop_announce_rounds() {
        let _ = announce_sock
            .send_to(&pkt, format!("127.0.0.1:{HOP_CONTROL_PORT}"))
            .await;
        if let Ok(Some(ch)) = tokio::time::timeout(hop_announce_interval(), ack_rx.recv()).await {
            if ch == target {
                acked = true;
                break;
            }
        }
    }
    if !acked {
        return;
    }
    sleep_to_epoch(epoch).await;
    proc.lock().await.kill_all().await;
    // A silent `iw set channel` failure makes the hop outcome false even when
    // the radio respawns cleanly: a hop that landed on the old channel is not a
    // successful hop. The radio is always respawned so the link is never left
    // dead.
    let channel_ok = set_channel(iface, target).await;
    match RadioProcesses::spawn(iface, cfg, Path::new(WFB_TX_KEY), link.clone()).await {
        Ok(new_proc) => {
            *proc.lock().await = new_proc;
            restart_count.fetch_add(1, Ordering::Relaxed);
            state.lock().await.record_hop(target, label, channel_ok);
            if channel_ok {
                tracing::info!(iface, channel = target, trigger = label, "hop_executed");
            } else {
                tracing::warn!(
                    iface,
                    channel = target,
                    trigger = label,
                    "hop_channel_unverified"
                );
            }
        }
        Err(e) => {
            state.lock().await.record_hop(target, label, false);
            tracing::warn!(error = %e, "hop_wfb_restart_failed");
        }
    }
}

/// Sleep until the hop epoch (wall-clock ms). No-op if the epoch is past.
async fn sleep_to_epoch(epoch_ms: u64) {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    let delay = (epoch_ms as f64 / 1000.0) - now_secs;
    if delay > 0.0 {
        tokio::time::sleep(Duration::from_secs_f64(delay)).await;
    }
}

/// True when a decoded beacon is this rig's own (a loopback race can deliver the
/// emitter's own PresenceBeacon to the listener). The beacon carries a 16-byte
/// device-id, so the check compares the beacon id against the first 16 bytes of
/// the own device-id (matching `manager.py`'s `own_device_id[:16]`). An empty
/// own device-id never matches (a rig without an id cannot self-collide).
fn is_self_beacon(own_device_id: &str, beacon_device_id: &str) -> bool {
    if own_device_id.is_empty() {
        return false;
    }
    let truncated: String = own_device_id.chars().take(16).collect();
    beacon_device_id == truncated
}

/// Signal a freshly-learned peer device-id for back-fill into the persisted pair
/// state. Writes `peer-backfill.json` = `{"peer_device_id": <id>}` atomically;
/// the REST seam reads it and calls `update_peer_device_id("drone", id)`, which
/// owns the config.yaml round-trip so the radio service never clobbers unrelated
/// config keys (the persisted-pair write stays on the API side that already owns
/// config persistence). The bind tunnel does not always carry the peer id, so
/// the presence beacon is the canonical source.
fn write_peer_backfill_json(peer_device_id: &str) {
    let v = json!({ "peer_device_id": peer_device_id });
    let _ = write_sidecar(&run_path("peer-backfill.json"), &v);
}

/// Write `peer-presence.json` (Contract E) from the shared hop state.
async fn write_peer_presence_json(state: &Arc<tokio::sync::Mutex<HopState>>) {
    let v = {
        let s = state.lock().await;
        match s.peer() {
            Some(p) => json!({
                "peer_device_id": p.device_id,
                "peer_role": p.role,
                "peer_channel": p.channel,
                "peer_rssi_dbm": p.rssi_dbm,
                "peer_last_seen_unix": s.peer_last_seen_unix(),
            }),
            None => json!({
                "peer_device_id": serde_json::Value::Null,
                "peer_role": serde_json::Value::Null,
                "peer_channel": serde_json::Value::Null,
                "peer_rssi_dbm": serde_json::Value::Null,
                "peer_last_seen_unix": serde_json::Value::Null,
            }),
        }
    };
    let _ = write_sidecar(&run_path("peer-presence.json"), &v);
}

/// Write `hop-supervisor.json` (Contract E) from the shared hop state + config.
/// `enabled_channels` is the regulatory-permitted channel set used to intersect
/// hop candidates; surfacing it lets the panel show why a hop was refused.
async fn write_hop_supervisor_json(
    state: &Arc<tokio::sync::Mutex<HopState>>,
    cfg: &WfbConfig,
    enabled_channels: &std::collections::BTreeSet<u8>,
) {
    let v = {
        let s = state.lock().await;
        let history =
            serde_json::to_value(s.history()).unwrap_or_else(|_| serde_json::Value::Array(vec![]));
        let enabled: Vec<u8> = enabled_channels.iter().copied().collect();
        json!({
            "enabled": cfg.auto_hop_enabled,
            "band": cfg.band,
            "hop_period_seconds": cfg.hop_period_seconds,
            "loss_threshold_percent": cfg.hop_loss_threshold_percent as f64,
            "rssi_threshold_dbm": cfg.hop_rssi_threshold_dbm as f64,
            "enabled_channels": enabled,
            "last_hop_at": s.last_hop_at_unix(),
            "history": history,
            "wall_time_unix": ados_radio::hop::now_unix(),
        })
    };
    let _ = write_sidecar(&run_path("hop-supervisor.json"), &v);
}

/// Per-call ceiling on the `iw set channel` + readback so a hung `iw` (driver
/// wedged mid-retune) cannot stall the hop / return-home path.
const SET_CHANNEL_TIMEOUT: Duration = Duration::from_secs(5);

/// `iw <iface> set channel <ch>`, VERIFIED. Returns `true` only when the
/// command exits 0 AND a readback of `iw <iface> info` confirms the interface
/// landed on `channel`. A silent driver no-op (exit 0 but the channel never
/// changed) and a hung `iw` both record `false` instead of a false success, so
/// the caller's hop / return-home outcome reflects reality.
async fn set_channel(iface: &str, channel: u8) -> bool {
    let status = tokio::time::timeout(
        SET_CHANNEL_TIMEOUT,
        tokio::process::Command::new("iw")
            .args([iface, "set", "channel", &channel.to_string()])
            .status(),
    )
    .await;
    match status {
        Ok(Ok(s)) if s.success() => {}
        Ok(Ok(s)) => {
            tracing::warn!(iface, channel, exit = s.code(), "iw_set_channel_failed");
            return false;
        }
        Ok(Err(e)) => {
            tracing::warn!(iface, channel, error = %e, "iw_set_channel_error");
            return false;
        }
        Err(_) => {
            tracing::warn!(iface, channel, "iw_set_channel_timeout");
            return false;
        }
    }
    // Read back the live channel; a mismatch (or unreadable info) is a failure.
    match channel_from_iface(iface).await {
        Some(live) if live == channel => true,
        Some(live) => {
            tracing::warn!(iface, channel, live, "iw_set_channel_readback_mismatch");
            false
        }
        None => {
            tracing::warn!(iface, channel, "iw_set_channel_readback_unavailable");
            false
        }
    }
}

/// Read the interface's current channel from `iw <iface> info`, or `None` when
/// `iw` cannot be run or its output carries no channel. Split out so the
/// readback parse is unit-testable independently of the subprocess.
async fn channel_from_iface(iface: &str) -> Option<u8> {
    let out = tokio::time::timeout(
        SET_CHANNEL_TIMEOUT,
        tokio::process::Command::new("iw")
            .args([iface, "info"])
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    parse_iface_channel(&String::from_utf8_lossy(&out.stdout))
}

/// Parse the `channel <N>` token out of an `iw <iface> info` body. The line
/// reads e.g. `\tchannel 149 (5745 MHz), width: 20 MHz, …`; the first integer
/// after the `channel` keyword is the channel number. Pure helper.
fn parse_iface_channel(info: &str) -> Option<u8> {
    for line in info.lines() {
        let mut toks = line.split_whitespace();
        while let Some(tok) = toks.next() {
            if tok == "channel" {
                if let Some(n) = toks.next() {
                    if let Ok(ch) = n.parse::<u8>() {
                        return Some(ch);
                    }
                }
            }
        }
    }
    None
}

/// The adapter facts the sidecar surfaces (None until an adapter is selected).
#[derive(Clone, Default)]
struct AdapterInfo {
    interface: String,
    chipset: String,
    injection_ok: bool,
    /// Enumerated USB link speed (Mbps); None when not USB / unreadable.
    usb_speed_mbps: Option<u32>,
    /// True when the adapter is on a slow USB link (full-speed) and so may
    /// advance tx_bytes while emitting no usable RF.
    usb_degraded: bool,
}

/// The truthful channel picture the sidecar surfaces, so the operator and the
/// GCS see where the radio ACTUALLY is, not where it was configured to be.
///
/// - `actual` is the LIVE channel read from `iw dev` this tick. Under a
///   forbidden domain the driver can land the interface on an in-band fallback
///   frequency; reporting the live value surfaces that instead of masking it
///   behind the configured channel.
/// - `rendezvous` is the operator's home / meeting channel (the immutable
///   `video.wfb.channel`, or the optional rendezvous pin). Both rigs derive it
///   identically, so it is the guaranteed meeting point.
/// - `operating` is the runtime channel (tmpfs); it equals `rendezvous` unless a
///   coordinated channel move committed.
/// - `locked` is the received-side lock proof — true only when a verified return
///   signal was heard, never hardcoded.
/// - `rf_unverified` is raised when the transmit counter is advancing yet no
///   return signal has been heard within the grace window (the
///   transmitting-zero-reception case).
#[derive(Clone, Copy, Default)]
struct ChannelTruth {
    actual: u8,
    rendezvous: u8,
    operating: u8,
    locked: bool,
    rf_unverified: bool,
}

impl ChannelTruth {
    /// Pre-bring-up truth, before the interface exists to read a live channel
    /// from: all three channels report the rendezvous home, the link is not yet
    /// proven, and the transmit counter is not advancing. Used for the
    /// unpaired / reg-blocked / no-adapter / connecting states the heartbeat has
    /// not yet refined with a live read.
    fn configured(rendezvous: u8) -> Self {
        Self {
            actual: rendezvous,
            rendezvous,
            operating: rendezvous,
            locked: false,
            rf_unverified: false,
        }
    }
}

/// The regulatory picture the sidecar surfaces, so a domain the global set could
/// not displace (the forbidden-band case) is visible in one glance instead of
/// masked. `domain` is the LIVE global country (`None` when unreadable);
/// `verified` is true only when it matched the wanted domain; `enabled_channels`
/// is the domain's permitted channel set (empty = could not determine).
#[derive(Clone, Default)]
struct RegSnapshot {
    domain: Option<String>,
    verified: bool,
    enabled_channels: Vec<u8>,
}

/// Compute the 16-hex-char public-key fingerprint of the drone TX key, or `None`
/// when the key is absent or not exactly 64 bytes. The peer-public half is the
/// second 32 bytes of the WFB key file; the fingerprint is `blake2b(pub,
/// digest_size=8)` rendered as 16 lowercase hex chars. Both rigs of a pair
/// compute the same value from their respective key files, so heartbeat
/// cross-checks reduce to a string compare. Byte-identical to
/// `key_mgr.read_public_fingerprint`.
fn read_public_fingerprint(path: &Path) -> Option<String> {
    use blake2::digest::{Update, VariableOutput};
    use blake2::Blake2bVar;
    const WFB_KEY_FILE_BYTES: usize = 64;
    const WFB_PUBLIC_HALF_OFFSET: usize = 32;
    let data = std::fs::read(path).ok()?;
    if data.len() != WFB_KEY_FILE_BYTES {
        return None;
    }
    let mut hasher = Blake2bVar::new(8).ok()?;
    hasher.update(&data[WFB_PUBLIC_HALF_OFFSET..]);
    let mut out = [0u8; 8];
    hasher.finalize_variable(&mut out).ok()?;
    Some(hex::encode(out))
}

/// Write the `wfb-stats.json` Contract E sidecar (full schema the REST handler
/// at `api/routes/wfb.py` merges over its base, so the GCS/LCD/dashboard radio
/// panel renders correctly). The link-quality fields (rssi/snr/packets/loss/
/// bitrate) are left to the REST base defaults until the link-quality monitor
/// lands; `adapter_chipset`/`adapter_injection_ok`/`tx_power_dbm` must be
/// present here or the panel shows a false "stranded radio" warning. Re-written
/// on a 2 s cadence so the handler's `mtime > 10 s → state="stale"` never trips.
///
/// Carries the pair block (`paired` + identity) read from the same on-disk
/// sources the Python `get_status` reads — the TX key for `paired` +
/// `public_key_fingerprint`, the `video.wfb` config for the peer id / paired-at
/// / auto-pair flag — plus the watchdog kill/stall counters. All key names match
/// `manager.get_status` exactly.
#[allow(clippy::too_many_arguments)]
fn write_stats_sidecar(
    state: &str,
    channels: &ChannelTruth,
    reg: &RegSnapshot,
    effective_tx_dbm: i8,
    adapter: Option<&AdapterInfo>,
    link: &LinkStats,
    cfg: &WfbConfig,
    restart_count: u64,
    counters: &WatchdogCounters,
    rates: &TxRates,
    bitrate: &BitrateSnapshot,
) {
    let (interface, chipset, injection_ok) = match adapter {
        Some(a) => (a.interface.as_str(), a.chipset.as_str(), a.injection_ok),
        None => ("", "", false),
    };
    let (adapter_usb_speed_mbps, adapter_usb_degraded) = match adapter {
        Some(a) => (a.usb_speed_mbps, a.usb_degraded),
        None => (None, false),
    };
    // Pair identity: the fingerprint + paired flag come from the TX key on disk,
    // the peer id / paired-at / auto-pair flag from the persisted config block.
    let fingerprint = read_public_fingerprint(Path::new(WFB_TX_KEY));
    let paired = fingerprint.is_some();
    let v = json!({
        "state": state,
        // The state-machine state, surfaced under its own key so the panel can
        // show the recovery state directly. Mirrors `state` (the same wire
        // vocabulary, including `reg_blocked`); kept distinct so a future
        // state-machine value never collides with the legacy `state` consumers.
        "link_state": state,
        "interface": interface,
        // Back-compat alias: `channel` now reflects the LIVE interface channel
        // (was the configured value). Readers that only know the old key still
        // get reality. The split-out actual/rendezvous/operating fields below
        // carry the full truth.
        "channel": channels.actual,
        "actual_channel": channels.actual,
        "rendezvous_channel": channels.rendezvous,
        "operating_channel": channels.operating,
        // Live regulatory picture: the domain actually in force, whether it
        // matched the wanted domain, and the permitted channel set. A forbidden
        // domain the global set could not displace shows here instead of being
        // masked by a configured-channel-and-locked report.
        "reg_domain": reg.domain,
        "reg_verified": reg.verified,
        "enabled_channels": reg.enabled_channels,
        // Transmitting yet no confirmed reception within the grace window — the
        // loose-antenna / forbidden-band-cap / dead-peer case. False while the
        // link is proven OR while the transmit counter is flat (idle).
        "rf_unverified": channels.rf_unverified,
        "adapter_chipset": chipset,
        "adapter_injection_ok": injection_ok,
        // USB link health of the selected adapter. A full-speed (12 Mbps)
        // enumeration on an RTL adapter means it can advance tx_bytes yet emit
        // no usable RF — surfaced so the GCS warns instead of showing "connected".
        "adapter_usb_speed_mbps": adapter_usb_speed_mbps,
        "adapter_usb_degraded": adapter_usb_degraded,
        "tx_power_dbm": effective_tx_dbm,
        "tx_power_max_dbm": cfg.tx_power_max_dbm,
        "topology": cfg.topology,
        "mcs_index": cfg.mcs_index,
        // Received-side lock proof, never hardcoded: a transmit-only end has no
        // decode stats of its own, so this is true only when a verified return
        // signal (a control-plane ack or a peer beacon) was heard recently.
        "channel_locked": channels.locked,
        "profile": "drone",
        // Count of radio-group respawns since service start (watchdog kills,
        // hop restarts, return-home restarts) — surfaces churn to the panel.
        "restart_count": restart_count,
        // Pair identity block (matches manager.get_status key-for-key) so the
        // GCS radio panel renders pair identity without the cloud relay.
        "paired": paired,
        "paired_with_device_id": cfg.paired_with_device_id,
        "paired_at": cfg.paired_at,
        "public_key_fingerprint": fingerprint,
        "auto_pair_enabled": cfg.auto_pair_enabled,
        // Watchdog kill/stall counters (the watchdogs detect these; surfaced
        // here so the panel sees the same churn the Python heartbeat reports).
        "tx_zombie_kills": counters.tx_zombie_kills,
        "tx_video_stalled": counters.tx_video_stalled,
        "tx_video_stall_kills": counters.tx_video_stall_kills,
        "tx_video_recvq_bytes": counters.tx_video_recvq_bytes,
        // Smoothed radio transmit rate; valid_rx_packets_per_s is the uplink
        // valid-decode rate (0 on a drone-only rig with no rx.key).
        "tx_bytes_per_s": (rates.tx_bytes_per_s * 10.0).round() / 10.0,
        "valid_rx_packets_per_s": (rates.valid_rx_packets_per_s * 100.0).round() / 100.0,
        // Adaptive bitrate / FEC controller intent. `recommended_bitrate_kbps`
        // is the controller's chosen rung bitrate; the actual encoder restart is
        // a cross-process no-op here (the encoder lives in another service), so
        // the panel shows controller intent regardless. `link_preset` is the
        // operator-facing preset that seeded the MCS/FEC trio at bring-up.
        "link_preset": bitrate.link_preset,
        "adaptive_bitrate_enabled": bitrate.adaptive_bitrate_enabled,
        "recommended_bitrate_kbps": bitrate.recommended_bitrate_kbps,
        // Link-quality block (from the stats wfb_rx; defaults until frames flow).
        "rssi_dbm": link.rssi_dbm,
        "rssi_min": link.rssi_min,
        "rssi_max": link.rssi_max,
        "noise_dbm": link.noise_dbm,
        "snr_db": link.snr_db,
        "packets_received": link.packets_received,
        "packets_lost": link.packets_lost,
        "fec_recovered": link.fec_recovered,
        "fec_failed": link.fec_failed,
        "bitrate_kbps": link.bitrate_kbps,
        "loss_percent": link.loss_percent,
        "timestamp": link.timestamp,
    });
    let path = run_path("wfb-stats.json");
    let _ = write_sidecar(&path, &v);
}

/// Read the device-id from the canonical agent location (`/etc/ados/device-id`,
/// hyphen — matches `core/paths.py:122 DEVICE_ID_PATH`).
fn read_device_id() -> String {
    std::fs::read_to_string("/etc/ados/device-id")
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Resolve when SIGTERM or SIGINT is received.
async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
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

#[cfg(test)]
mod tests {
    use super::*;
    use ados_radio::hop::HopState;

    fn tripping_link() -> LinkStats {
        // Real data: non-empty timestamp + packets flowing, with loss/RSSI past
        // the default thresholds (loss 20% > 10, rssi -80 < -75).
        LinkStats {
            timestamp: "2026-05-30T00:00:00+00:00".to_string(),
            packets_received: 500,
            loss_percent: 20.0,
            rssi_dbm: -80.0,
            ..LinkStats::default()
        }
    }

    /// The hop-period floor applied at the hop-supervisor tick build site. A
    /// scan locks the radio for several seconds, so the period must never fall
    /// below the floor (and a 0 would panic `interval`). Pure to mirror the
    /// inline `.max(HOP_PERIOD_FLOOR_SECS)` without standing up the supervisor.
    fn floored_hop_period(configured: u32) -> u64 {
        const HOP_PERIOD_FLOOR_SECS: u64 = 15;
        (configured as u64).max(HOP_PERIOD_FLOOR_SECS)
    }

    #[test]
    fn hop_period_floor_clamps_low_values() {
        // A zero (which would panic tokio::time::interval) and any sub-floor
        // value clamp UP to the 15 s floor.
        assert_eq!(floored_hop_period(0), 15);
        assert_eq!(floored_hop_period(1), 15);
        assert_eq!(floored_hop_period(14), 15);
        // Exactly the floor stays the floor.
        assert_eq!(floored_hop_period(15), 15);
    }

    #[test]
    fn hop_period_floor_leaves_longer_periods_untouched() {
        // The default (60 s) and any value above the floor pass through.
        assert_eq!(floored_hop_period(16), 16);
        assert_eq!(floored_hop_period(60), 60);
        assert_eq!(floored_hop_period(300), 300);
    }

    #[test]
    fn default_link_stats_never_fires_reactive() {
        // The default LinkStats has rssi -100 (< -75) but no real data (empty
        // timestamp, 0 packets) so the gate must NOT fire. This is the
        // drone-only-rig case that would otherwise hop every cycle forever.
        let link = LinkStats::default();
        assert!(!reactive_should_fire(true, &link, 10.0, -75.0));
    }

    #[test]
    fn reg_gate_ok_proceeds() {
        let ok: Result<(), adapter::RegError> = Ok(());
        assert_eq!(decide_reg_gate(&ok, true), RegGateDecision::Proceed);
        assert_eq!(decide_reg_gate(&ok, false), RegGateDecision::Proceed);
    }

    #[test]
    fn reg_gate_strict_failure_blocks_with_reason() {
        let err: Result<(), adapter::RegError> =
            Err(adapter::RegError::ChannelNotEnabled { channel: 165 });
        assert_eq!(
            decide_reg_gate(&err, true),
            RegGateDecision::Block {
                reason: "channel_not_enabled"
            }
        );
    }

    #[test]
    fn reg_gate_eeprom_override_blocks_under_strict() {
        // The live override case: phy bakes a different country than wanted.
        let err: Result<(), adapter::RegError> = Err(adapter::RegError::EepromOverride {
            want: "US".into(),
            got: "BO".into(),
        });
        assert_eq!(
            decide_reg_gate(&err, true),
            RegGateDecision::Block {
                reason: "phy_override"
            }
        );
    }

    #[test]
    fn reg_gate_failure_proceeds_best_effort_when_not_strict() {
        // The lab escape hatch (reg_gate_strict: false) proceeds anyway.
        let err: Result<(), adapter::RegError> = Err(adapter::RegError::VerifyTimeout {
            want: "US".into(),
            got: Some("BO".into()),
        });
        assert_eq!(
            decide_reg_gate(&err, false),
            RegGateDecision::ProceedBestEffort {
                reason: "verify_timeout"
            }
        );
    }

    #[test]
    fn reg_blocked_state_string_is_bland_and_stable() {
        // The sidecar surfaces this verbatim; keep it stable and tag-free.
        assert_eq!(STATE_REG_BLOCKED, "reg_blocked");
    }

    #[test]
    fn parse_iface_channel_reads_channel_token() {
        // The readback seam set_channel uses to verify the live channel. The
        // verified-bool is `set_ok && parse == target`; here we exercise the
        // parse half so a silent driver no-op (info still shows the old channel)
        // is distinguishable from a real retune.
        let info = "Interface wlan1\n\tifindex 5\n\ttype monitor\n\
                    \tchannel 149 (5745 MHz), width: 20 MHz, center1: 5745 MHz\n";
        assert_eq!(parse_iface_channel(info), Some(149));
        // A different live channel parses to its own value, so a mismatch
        // against the requested target records ok=false.
        let other = "Interface wlan1\n\tchannel 36 (5180 MHz), width: 20 MHz\n";
        assert_eq!(parse_iface_channel(other), Some(36));
    }

    #[test]
    fn parse_iface_channel_no_channel_is_none() {
        // No `channel` line (radio not on a channel, or unreadable info) → None,
        // which set_channel treats as an unverified failure (ok=false).
        assert_eq!(
            parse_iface_channel("Interface wlan1\n\ttype managed\n"),
            None
        );
        assert_eq!(parse_iface_channel(""), None);
        // A bare `channel` keyword with no number is also None, not a panic.
        assert_eq!(parse_iface_channel("\tchannel\n"), None);
    }

    #[test]
    fn real_data_over_threshold_fires_once_then_cooldown_blocks() {
        let link = tripping_link();
        // First fire: cooldown allowed + real data over threshold.
        assert!(reactive_should_fire(true, &link, 10.0, -75.0));
        // Cooldown not yet met (a hop just happened) so blocked even though the
        // link still trips the thresholds.
        assert!(!reactive_should_fire(false, &link, 10.0, -75.0));
    }

    #[test]
    fn real_data_under_threshold_does_not_fire() {
        // Real data but a healthy link (low loss, strong RSSI) so no reactive hop.
        let link = LinkStats {
            timestamp: "2026-05-30T00:00:00+00:00".to_string(),
            packets_received: 500,
            loss_percent: 2.0,
            rssi_dbm: -55.0,
            ..LinkStats::default()
        };
        assert!(!reactive_should_fire(true, &link, 10.0, -75.0));
    }

    #[test]
    fn reactive_cooldown_observed_through_hop_state() {
        // A freshly recorded hop blocks the next reactive for 30 s. Pair the
        // HopState cooldown (reactive_allowed) with the predicate to confirm the
        // loop's actual gate respects the cooldown.
        let mut s = HopState::new(149);
        s.on_peer_seen();
        // Before any hop the cooldown is met (None last_hop_at).
        assert!(s.reactive_allowed());
        assert!(reactive_should_fire(
            s.reactive_allowed(),
            &tripping_link(),
            10.0,
            -75.0
        ));
        // Record a hop so the 30 s cooldown starts and reactive is blocked.
        s.record_hop(153, "reactive", true);
        assert!(!s.reactive_allowed());
        assert!(!reactive_should_fire(
            s.reactive_allowed(),
            &tripping_link(),
            10.0,
            -75.0
        ));
    }

    #[test]
    fn fresh_peer_periodic_skips_scan_unseen_does_not() {
        // The periodic arm skips the scan when the peer is fresh (<60 s); a peer
        // never seen lets the scan run. peer_fresh_within is the gate the arm
        // reads (using only the public HopState surface).
        let never = HopState::new(149);
        assert!(!never.peer_fresh_within(PEER_FRESH_SKIP_SECS));

        let mut seen = HopState::new(149);
        seen.on_peer_seen();
        // Just-seen peer is fresh so the periodic scan is skipped.
        assert!(seen.peer_fresh_within(PEER_FRESH_SKIP_SECS));
    }

    #[test]
    fn tx_liveness_tracks_counter_progress() {
        let mut live = TxLiveness::new();
        // No reading yet so not live.
        assert!(!live.tx_live());
        // First reading seeds the baseline (does not count as a change).
        live.observe(1000);
        assert!(live.tx_live()); // value > 0 and last_change is "now"
                                 // A zero counter is never live even if it just changed.
        let mut zero = TxLiveness::new();
        zero.observe(0);
        assert!(!zero.tx_live());
    }

    #[test]
    fn tx_liveness_rate_zero_before_second_reading() {
        let mut live = TxLiveness::new();
        // The seeding read produces no rate yet.
        live.observe(1000);
        assert_eq!(live.tx_bytes_per_s(), 0.0);
    }

    #[test]
    fn tx_liveness_rate_clamps_counter_reset() {
        let mut live = TxLiveness::new();
        live.observe(5000);
        // A counter that goes BACKWARDS (iface reset / wrap) must not produce a
        // negative rate — saturating_sub clamps the delta to 0.
        live.observe(100);
        assert_eq!(live.tx_bytes_per_s(), 0.0);
    }

    #[test]
    fn self_beacon_matches_first_16_bytes() {
        // A loopback copy of this rig's own beacon carries the first 16 bytes of
        // its device-id; the listener must drop it.
        let own = "0123456789abcdef0123"; // 20 chars
        assert!(is_self_beacon(own, "0123456789abcdef"));
        // A different device-id is a real peer, never dropped.
        assert!(!is_self_beacon(own, "fedcba9876543210"));
        // A short own id (≤16) matches itself verbatim.
        assert!(is_self_beacon("abc123", "abc123"));
        // An empty own id can never self-collide.
        assert!(!is_self_beacon("", ""));
        assert!(!is_self_beacon("", "anything"));
    }

    #[test]
    fn fingerprint_none_when_key_absent_or_wrong_size() {
        let dir = tempfile::tempdir().unwrap();
        // Missing file → None.
        assert!(read_public_fingerprint(&dir.path().join("nope.key")).is_none());
        // Wrong size (not 64 bytes) → None.
        let short = dir.path().join("short.key");
        std::fs::write(&short, vec![0u8; 32]).unwrap();
        assert!(read_public_fingerprint(&short).is_none());
    }

    #[test]
    fn fingerprint_is_16_hex_of_blake2b_8_over_public_half() {
        use blake2::digest::{Update, VariableOutput};
        use blake2::Blake2bVar;
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("tx.key");
        // 64-byte key: first 32 are the secret half, second 32 the public half.
        let mut data = vec![0u8; 64];
        for (i, b) in data.iter_mut().enumerate() {
            *b = i as u8;
        }
        std::fs::write(&key, &data).unwrap();
        let got = read_public_fingerprint(&key).unwrap();
        // Recompute independently over the second 32 bytes.
        let mut h = Blake2bVar::new(8).unwrap();
        h.update(&data[32..]);
        let mut out = [0u8; 8];
        h.finalize_variable(&mut out).unwrap();
        assert_eq!(got, hex::encode(out));
        assert_eq!(got.len(), 16);
        assert!(got
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn channel_truth_configured_reports_rendezvous_for_all_channels() {
        // Before the interface exists, the truth reports the rendezvous home for
        // actual/rendezvous/operating, with the link not proven and no tx.
        let t = ChannelTruth::configured(149);
        assert_eq!(t.actual, 149);
        assert_eq!(t.rendezvous, 149);
        assert_eq!(t.operating, 149);
        assert!(!t.locked);
        assert!(!t.rf_unverified);
    }

    /// Serialize tests that mutate the process-global `ADOS_RUN_DIR` env var.
    static SIDECAR_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn read_sidecar(dir: &std::path::Path) -> serde_json::Value {
        let body = std::fs::read(dir.join("wfb-stats.json")).unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    #[test]
    fn sidecar_carries_truthful_channel_and_reg_fields() {
        let _guard = SIDECAR_ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("ADOS_RUN_DIR", dir.path());

        // A locked link on a live fallback channel under a verified domain: the
        // actual channel differs from the rendezvous home (the fallback-frequency
        // case), the link is proven, and rf_unverified is clear.
        let channels = ChannelTruth {
            actual: 161,
            rendezvous: 149,
            operating: 157,
            locked: true,
            rf_unverified: false,
        };
        let reg = RegSnapshot {
            domain: Some("US".to_string()),
            verified: true,
            enabled_channels: vec![149, 153, 157, 161, 165],
        };
        write_stats_sidecar(
            "connected",
            &channels,
            &reg,
            5,
            None,
            &LinkStats::default(),
            &WfbConfig::default(),
            0,
            &WatchdogCounters::default(),
            &TxRates::default(),
            &BitrateSnapshot::default(),
        );
        let v = read_sidecar(dir.path());

        // The back-compat `channel` alias now equals the LIVE actual channel.
        assert_eq!(v["channel"], 161);
        assert_eq!(v["actual_channel"], 161);
        assert_eq!(v["rendezvous_channel"], 149);
        assert_eq!(v["operating_channel"], 157);
        assert_eq!(v["reg_domain"], "US");
        assert_eq!(v["reg_verified"], true);
        assert_eq!(
            v["enabled_channels"],
            serde_json::json!([149, 153, 157, 161, 165])
        );
        // channel_locked is the proof-derived value, not hardcoded true.
        assert_eq!(v["channel_locked"], true);
        assert_eq!(v["rf_unverified"], false);
        // link_state mirrors the lifecycle state string.
        assert_eq!(v["link_state"], "connected");

        std::env::remove_var("ADOS_RUN_DIR");
    }

    #[test]
    fn sidecar_reports_rf_unverified_and_unlocked_when_transmitting_blind() {
        let _guard = SIDECAR_ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("ADOS_RUN_DIR", dir.path());

        // Transmitting (tx advancing) but no confirmed reception: locked false,
        // rf_unverified true — the exact transmitting-zero-reception case.
        let channels = ChannelTruth {
            actual: 149,
            rendezvous: 149,
            operating: 149,
            locked: false,
            rf_unverified: true,
        };
        let reg = RegSnapshot {
            domain: Some("BO".to_string()),
            verified: false,
            enabled_channels: vec![],
        };
        write_stats_sidecar(
            STATE_REG_BLOCKED,
            &channels,
            &reg,
            5,
            None,
            &LinkStats::default(),
            &WfbConfig::default(),
            0,
            &WatchdogCounters::default(),
            &TxRates::default(),
            &BitrateSnapshot::default(),
        );
        let v = read_sidecar(dir.path());
        assert_eq!(v["channel_locked"], false);
        assert_eq!(v["rf_unverified"], true);
        // The forbidden domain the global set could not displace is visible.
        assert_eq!(v["reg_domain"], "BO");
        assert_eq!(v["reg_verified"], false);
        assert_eq!(v["enabled_channels"], serde_json::json!([]));
        assert_eq!(v["state"], "reg_blocked");

        std::env::remove_var("ADOS_RUN_DIR");
    }
}
