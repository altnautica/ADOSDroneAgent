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

// Focused sibling modules the orchestrator (`main` + `run_service`) drives.
// Each groups one concern lifted out of this file; the binary behaves
// identically — the helpers are simply re-homed and called through `use`.
//
// `backend` is the pluggable radio-backend seam. It is a
// PURE-ADD: built + unit-tested but NOT yet wired into `run_service` (the live
// bring-up still runs inline below), so declaring it here changes no behaviour.
mod backend;
mod bringup;
mod hop_supervisor;
mod reg_gate;
mod sidecar;
mod txrate;

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{watch, Notify};

use ados_radio::adapter;
use ados_radio::aux_cmd::{self, AuxCmdState};
use ados_radio::bitrate::{
    new_enabled, new_snapshot, BitrateController, EnabledHandle, SnapshotHandle,
};
use ados_radio::cmdsock::{self, CmdState};
use ados_radio::config::WfbConfig;
use ados_radio::hop::derive_pair_key;
use ados_radio::link_quality::LinkStats;
use ados_radio::link_state::derive_link_state;
use ados_radio::paths::{read_bind_sentinel_active, DRONE_KEY, WFB_TX_KEY};
use ados_radio::process::RadioProcesses;
use ados_radio::watchdog::{
    aux_liveness_watchdog, new_counters, tx_health_watchdog, video_recvq_watchdog, CounterHandle,
    WatchdogCounters, WatchdogFired,
};

use bringup::{channel_from_iface, ensure_monitor_and_channel, ensure_radiating};
use hop_supervisor::{emit_presence_beacons, proof_only_listener, run_hop_supervisor};
use reg_gate::{decide_reg_gate, RegGateDecision, REG_BLOCKED_RETRY_SECS, STATE_REG_BLOCKED};
use sidecar::{
    build_stats_value, json_object_to_fields, read_device_id, write_adapters_sidecar,
    write_stats_sidecar, AdapterInfo, ChannelTruth, RegPosture, RegSnapshot,
};
use txrate::{read_tx_bytes, TxLiveness, TxRates};

const CONFIG_YAML: &str = "/etc/ados/config.yaml";
const PROFILE_CONF: &str = "/etc/ados/profile.conf";
/// Poll interval while waiting for the WFB TX key (unpaired state).
const KEY_WAIT_INTERVAL: Duration = Duration::from_secs(5);

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

    // Shutdown is a latching watch flag, not a one-shot `Notify`: once SIGTERM
    // flips it to `true` the value STAYS set, so a select arm that loses a race
    // on the first signal (e.g. a watchdog task finishing in the same poll) still
    // sees the shutdown on the next loop iteration, and any later SIGTERM is a
    // no-op rather than a lost edge. The same latching-watch pattern the auto-pair
    // supervisor uses for its own shutdown.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // ── Signal handler ────────────────────────────────────────────────────
    tokio::spawn(async move {
        wait_for_shutdown().await;
        // Send always succeeds while the receiver lives; if the service already
        // returned there is nothing left to signal.
        let _ = shutdown_tx.send(true);
    });

    run_service(&cfg, shutdown_rx).await;
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

async fn run_service(cfg: &WfbConfig, mut shutdown: watch::Receiver<bool>) {
    // Operating-region posture, read once at service start. The default is
    // unrestricted: the radio brings up + TX-enables on the home channel without
    // a verified operating region, and the operator is responsible for local RF
    // compliance. Pinning a region re-enables the strict regulatory gate for that
    // jurisdiction. This is the higher-level switch that gates the underlying
    // `reg_gate_strict` / `reg_domain` knobs; it never lifts the power-budget /
    // brownout clamp (tx_power_dbm / the muted-readback rf_unverified detector
    // stay armed in both postures).
    let reg_cfg = ados_radio::config::RegulatoryConfig::load_from(Path::new(CONFIG_YAML));
    let unrestricted = reg_cfg.mode.is_unrestricted();
    // The wanted domain the bring-up + global reconciler target. Region → the
    // pinned region code; unrestricted → the configured `reg_domain` fallback so
    // the global cfg80211 reconciler still keeps a sane domain (never `00`).
    let reg_fallback = cfg.reg_domain.clone().unwrap_or_else(|| "US".to_string());
    let region_domain = reg_cfg.wanted_domain(&reg_fallback).to_string();
    // The posture surfaced on every sidecar (even unpaired / no-adapter), so the
    // GCS / OLED / webapp always show the honest operating-region state.
    let reg_posture = RegPosture::new(unrestricted, &region_domain);
    if unrestricted {
        tracing::info!(
            note = "operating region not pinned; radiating at hardware-bounded power; operator responsible for local RF compliance",
            "wfb_unrestricted_posture"
        );
    } else {
        tracing::info!(region = %region_domain, "wfb_region_pinned");
    }

    // Event emitter for discrete, queryable verdicts shipped to the logging
    // daemon (the regulatory-gate decision, the received-side rf-unverified
    // state change). Best-effort and non-blocking: an absent daemon socket is
    // dropped quietly, never stalling the radio. The log lines + sidecar stay.
    let events = ados_protocol::logd::emitter::EventEmitter::new("ados-radio");
    // Telemetry emitter for the periodic link-quality samples shipped to the
    // logging daemon (RSSI / SNR / uncorrected-FEC, one set per heartbeat).
    // Same non-blocking, best-effort transport as the event emitter; a saturated
    // channel or an absent daemon drops the low-severity sample and counts it.
    let metrics = ados_protocol::logd::emitter::IngestEmitter::new("ados-radio");
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
        // Latched shutdown gate at the top of the respawn loop: if SIGTERM flipped
        // the watch while we were tearing down a radio group below, never start
        // another bring-up — return before re-spawning into a stopping service.
        if *shutdown.borrow() {
            return;
        }
        // ── Key guard — block while unpaired ─────────────────────────────
        if !Path::new(WFB_TX_KEY).exists() {
            tracing::info!(key = WFB_TX_KEY, "wfb_blocked_unpaired");
            write_stats_sidecar(
                "unpaired",
                &ChannelTruth::configured(cfg.rendezvous_channel()),
                &RegSnapshot {
                    posture: reg_posture.clone(),
                    ..RegSnapshot::default()
                },
                cfg.tx_power_dbm,
                None,
                &LinkStats::default(),
                cfg,
                restart_count.load(Ordering::Relaxed),
                &WatchdogCounters::default(),
                &TxRates::default(),
                &bitrate_snapshot.lock().await.clone(),
                Some(&metrics),
            );
            tokio::select! {
                biased;
                _ = wait_for_shutdown_flag(&mut shutdown) => return,
                _ = tokio::time::sleep(KEY_WAIT_INTERVAL) => continue,
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
            use ados_radio::reg_event::{
                is_eeprom_override, reg_gate_detail, RegGateResult, RegGateStage, REG_GATE_KIND,
            };
            // Unrestricted → set an EMPTY domain (set_reg_domain no-ops on ""), so
            // the bring-up does not force a country onto the injection PHY; the
            // global cfg80211 reconciler in the supervisor keeps the GLOBAL domain
            // sane independently. Region → the pinned region code.
            let domain = if unrestricted {
                ""
            } else {
                region_domain.as_str()
            };
            let reg_result = ados_radio::adapter::set_reg_domain(domain).await;
            let eeprom_override = reg_result
                .as_ref()
                .err()
                .map(is_eeprom_override)
                .unwrap_or(false);
            // Under the unrestricted posture the gate never blocks: strictness is
            // off regardless of the raw `reg_gate_strict` flag (the mode gates it).
            match decide_reg_gate(&reg_result, !unrestricted && cfg.reg_gate_strict) {
                RegGateDecision::Proceed => {
                    events.emit(
                        REG_GATE_KIND,
                        RegGateResult::Ok.severity(),
                        reg_gate_detail(
                            RegGateStage::Domain,
                            &cfg.band,
                            rendezvous_ch,
                            domain,
                            Some(domain),
                            false,
                            RegGateResult::Ok,
                            None,
                        ),
                    );
                }
                RegGateDecision::ProceedBestEffort { reason } => {
                    tracing::warn!(domain, reason, "wfb_reg_gate_proceeding_best_effort");
                    events.emit(
                        REG_GATE_KIND,
                        RegGateResult::Failed.severity(),
                        reg_gate_detail(
                            RegGateStage::Domain,
                            &cfg.band,
                            rendezvous_ch,
                            domain,
                            None,
                            eeprom_override,
                            RegGateResult::Failed,
                            Some(reason),
                        ),
                    );
                }
                RegGateDecision::Block { reason } => {
                    tracing::error!(domain, reason, "wfb_reg_gate_blocked");
                    // Surface the live domain vs the wanted one so the panel shows
                    // the actual conflict (e.g. a baked country the global set
                    // could not displace), not a configured-and-locked lie.
                    let status = ados_radio::adapter::read_reg_status(domain).await;
                    events.emit(
                        REG_GATE_KIND,
                        RegGateResult::Blocked.severity(),
                        reg_gate_detail(
                            RegGateStage::Domain,
                            &cfg.band,
                            rendezvous_ch,
                            domain,
                            status.domain.as_deref(),
                            eeprom_override,
                            RegGateResult::Blocked,
                            Some(reason),
                        ),
                    );
                    write_stats_sidecar(
                        STATE_REG_BLOCKED,
                        &ChannelTruth::configured(rendezvous_ch),
                        &RegSnapshot {
                            domain: status.domain,
                            verified: status.verified,
                            enabled_channels: Vec::new(),
                            posture: reg_posture.clone(),
                        },
                        cfg.tx_power_dbm,
                        None,
                        &LinkStats::default(),
                        cfg,
                        restart_count.load(Ordering::Relaxed),
                        &WatchdogCounters::default(),
                        &TxRates::default(),
                        &bitrate_snapshot.lock().await.clone(),
                        Some(&metrics),
                    );
                    tokio::select! {
                        biased;
                        _ = wait_for_shutdown_flag(&mut shutdown) => return,
                        _ = tokio::time::sleep(Duration::from_secs(REG_BLOCKED_RETRY_SECS)) => continue,
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
                &RegSnapshot {
                    posture: reg_posture.clone(),
                    ..RegSnapshot::default()
                },
                cfg.tx_power_dbm,
                None, // None adapter → chipset "" + adapter_injection_ok false
                &LinkStats::default(),
                cfg,
                restart_count.load(Ordering::Relaxed),
                &WatchdogCounters::default(),
                &TxRates::default(),
                &bitrate_snapshot.lock().await.clone(),
                Some(&metrics),
            );
            tokio::select! {
                biased;
                _ = wait_for_shutdown_flag(&mut shutdown) => return,
                _ = tokio::time::sleep(Duration::from_secs(10)) => continue,
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
            use ados_radio::reg_event::{
                reg_gate_detail, RegGateResult, RegGateStage, REG_GATE_KIND,
            };
            let domain = if unrestricted {
                ""
            } else {
                region_domain.as_str()
            };
            // Unrestricted → skip the channel-enabled / non-DFS assertion entirely
            // (the operator owns local compliance); the rendezvous channel is used
            // as-is. Region → today's strict channel readiness check.
            let ready = if unrestricted {
                Ok(())
            } else {
                adapter::assert_reg_ready(rendezvous_ch, &gate_enabled, &gate_dfs, cfg.dfs_allowed)
            };
            match decide_reg_gate(&ready, !unrestricted && cfg.reg_gate_strict) {
                RegGateDecision::Proceed => {
                    events.emit(
                        REG_GATE_KIND,
                        RegGateResult::Ok.severity(),
                        reg_gate_detail(
                            RegGateStage::Channel,
                            &cfg.band,
                            rendezvous_ch,
                            domain,
                            None,
                            false,
                            RegGateResult::Ok,
                            None,
                        ),
                    );
                }
                RegGateDecision::ProceedBestEffort { reason } => {
                    tracing::warn!(
                        channel = rendezvous_ch,
                        reason,
                        "wfb_reg_gate_channel_proceeding_best_effort"
                    );
                    let status = ados_radio::adapter::read_reg_status(domain).await;
                    events.emit(
                        REG_GATE_KIND,
                        RegGateResult::Failed.severity(),
                        reg_gate_detail(
                            RegGateStage::Channel,
                            &cfg.band,
                            rendezvous_ch,
                            domain,
                            status.domain.as_deref(),
                            false,
                            RegGateResult::Failed,
                            Some(reason),
                        ),
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
                    let status = ados_radio::adapter::read_reg_status(domain).await;
                    events.emit(
                        REG_GATE_KIND,
                        RegGateResult::Blocked.severity(),
                        reg_gate_detail(
                            RegGateStage::Channel,
                            &cfg.band,
                            rendezvous_ch,
                            domain,
                            status.domain.as_deref(),
                            false,
                            RegGateResult::Blocked,
                            Some(reason),
                        ),
                    );
                    write_stats_sidecar(
                        STATE_REG_BLOCKED,
                        &ChannelTruth::configured(rendezvous_ch),
                        &RegSnapshot {
                            domain: status.domain,
                            verified: status.verified,
                            enabled_channels: gate_enabled.iter().copied().collect(),
                            posture: reg_posture.clone(),
                        },
                        cfg.tx_power_dbm,
                        None,
                        &LinkStats::default(),
                        cfg,
                        restart_count.load(Ordering::Relaxed),
                        &WatchdogCounters::default(),
                        &TxRates::default(),
                        &bitrate_snapshot.lock().await.clone(),
                        Some(&metrics),
                    );
                    tokio::select! {
                        biased;
                        _ = wait_for_shutdown_flag(&mut shutdown) => return,
                        _ = tokio::time::sleep(Duration::from_secs(REG_BLOCKED_RETRY_SECS)) => continue,
                    }
                }
            }
        }
        // Re-assert monitor mode and land on the channel before bring-up
        // proceeds. A self-managed injection PHY (the RTL family) — or a
        // concurrent regulatory-domain set on the same wiphy — can revert the
        // vif to managed between adapter selection and here; a managed interface
        // rejects `iw set channel` with EBUSY. Spawning wfb_tx on a managed /
        // mis-tuned interface advances tx_bytes while radiating nothing a ground
        // station can decode (advancing tx_bytes with zero usable RF), so the
        // channel must land — verified — before TX starts. On total failure,
        // re-enter the selection loop rather than start a dead TX.
        if !ensure_monitor_and_channel(iface, cfg.channel).await {
            tracing::error!(
                iface,
                channel = cfg.channel,
                note = "monitor mode + channel never landed (EBUSY); not starting TX, retrying bring-up",
                "wfb_channel_set_unrecovered"
            );
            tokio::select! {
                biased;
                _ = wait_for_shutdown_flag(&mut shutdown) => return,
                _ = tokio::time::sleep(Duration::from_secs(REG_BLOCKED_RETRY_SECS)) => continue,
            }
        }

        // ── Regulatory re-assert (prevention layer) ──────────────────────
        // A self-managed injection PHY (the RTL family) can leave its
        // EEPROM-baked country as the GLOBAL regulatory domain after the
        // monitor-mode bring-up churn above. A normal onboard FullMAC adapter
        // then obeys that domain and can keep its association yet lose its data
        // path (the management-WiFi break with no failover). Re-assert the
        // configured wanted domain now, right after the churn, so the baked
        // country never lingers as the effective global. SAFETY: only force a
        // domain that permits the rendezvous channel — reuse the same channel
        // gate the bring-up just passed so this can never cap the radio. The
        // reactive WiFi self-heal (in the supervisor) stays as the backstop.
        {
            use ados_radio::adapter::ReassertOutcome;
            use ados_radio::reg_reassert::{
                reg_reassert_detail, REG_REASSERT_KIND, REG_REASSERT_SEVERITY,
            };
            // Keep the GLOBAL domain sane to protect the onboard management WiFi
            // (this is the prevention layer, orthogonal to the WFB-TX gate above).
            // Under the unrestricted posture this still re-asserts the configured
            // fallback domain (never `00`), so a self-managed baked country can
            // never linger as the effective global and strand the onboard WiFi.
            let wanted = region_domain.as_str();
            // The wanted domain must permit the rendezvous channel before we
            // force it. The bring-up gate read this set for THIS adapter already;
            // reuse it (no second `iw phy channels` call).
            let channel_ok =
                adapter::assert_reg_ready(rendezvous_ch, &gate_enabled, &gate_dfs, cfg.dfs_allowed)
                    .is_ok();
            match ados_radio::adapter::reconcile_reg_domain(wanted, rendezvous_ch, channel_ok).await
            {
                ReassertOutcome::Reasserted { from, to, .. } => {
                    events.emit(
                        REG_REASSERT_KIND,
                        REG_REASSERT_SEVERITY,
                        reg_reassert_detail(iface, from.as_deref(), &to, rendezvous_ch, true),
                    );
                }
                // In-sync / no-wanted / channel-unsafe: no durable event. The
                // skip path already logged a warning inside reconcile_reg_domain.
                ReassertOutcome::InSync
                | ReassertOutcome::NoWanted
                | ReassertOutcome::SkippedChannelUnsafe => {}
            }
        }

        // ── Clamp TX power BEFORE wfb_tx starts injecting ─────────────────
        // Critical on host-VBUS rigs: the driver default (~17-20 dBm) browns
        // out the adapter. Ramps up from the configured floor on rejection. The
        // power-budget clamp is NEVER lifted by the unrestricted posture — only
        // the muted-readback handling differs (surface vs abort, see
        // set_tx_power_modal): under unrestricted a muted PHY surfaces as
        // rf_unverified rather than aborting bring-up.
        let effective_tx_dbm = match ensure_radiating(
            iface,
            cfg.channel,
            cfg.tx_power_dbm,
            unrestricted,
        )
        .await
        {
            Some(dbm) => dbm,
            None => {
                // The PHY stayed pinned at the muted not-permitted floor through
                // every recovery attempt. Starting wfb_tx now injects into a dead
                // PHY (every sendmsg returns ENOBUFS, tx_bytes frozen) and the
                // liveness watchdog kill-loops it forever with no effect. Park and
                // re-enter bring-up instead of starting a TX that cannot radiate.
                tracing::error!(
                    iface,
                    channel = cfg.channel,
                    note = "PHY muted at txpower floor after recovery; not starting TX, retrying bring-up",
                    "wfb_phy_muted_unrecovered"
                );
                tokio::select! {
                    biased;
                    _ = wait_for_shutdown_flag(&mut shutdown) => return,
                    _ = tokio::time::sleep(Duration::from_secs(REG_BLOCKED_RETRY_SECS)) => continue,
                }
            }
        };

        // ── Load pair key for HMAC derivation ────────────────────────────
        let drone_key = tokio::fs::read(DRONE_KEY).await.ok();
        let pair_key = derive_pair_key(drone_key.as_deref());

        // ── Regulatory-enabled channel set, for the hop target filter ─────
        // Channels this adapter's reg domain forbids fail `iw set channel` with
        // -22 and split the pair onto divergent frequencies; the hop loop
        // intersects its candidates with this set. Empty = "could not
        // determine" → do not restrict. Reuse the set the regulatory gate
        // already read for this adapter bring-up (no second `iw` call). Under the
        // unrestricted posture the hop filter is disabled (empty set) so the hop
        // loop may target any in-band channel — the operator owns compliance.
        let enabled_channels = Arc::new(if unrestricted {
            std::collections::BTreeSet::new()
        } else {
            gate_enabled
        });
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
        // Region → the pinned region; unrestricted → the configured fallback (the
        // global reconciler keeps that sane, so `verified` still means something).
        let wanted_domain = region_domain.clone();

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
                    biased;
                    _ = wait_for_shutdown_flag(&mut shutdown) => return,
                    _ = tokio::time::sleep(Duration::from_secs(5)) => continue,
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
            posture: reg_posture.clone(),
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
            Some(&metrics),
        );
        tracing::info!(iface, channel = cfg.channel, pid, "wfb_service_ready");

        // ── Run watchdogs + hop supervisor concurrently ──────────────────
        // Per-bring-up cancel for the worker tasks. Each worker is also aborted
        // explicitly on respawn/shutdown below, so this `Notify` is the graceful
        // wake; a small bridge task fires it once the latched shutdown watch flips
        // so a worker's own `cancel.notified()` arm wins promptly. The bridge is
        // aborted alongside the workers, never outliving the bring-up.
        let task_cancel = Arc::new(Notify::new());
        let cancel_bridge = {
            let task_cancel = task_cancel.clone();
            let mut bridge_shutdown = shutdown.clone();
            tokio::spawn(async move {
                let _ = bridge_shutdown.wait_for(|s| *s).await;
                task_cancel.notify_waiters();
            })
        };
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
        let hb_reg_posture = reg_posture.clone();
        let hb_events = events.clone();
        let hb_metrics = metrics.clone();
        let hb_usb_speed = adapter_info.usb_speed_mbps;
        let mut heartbeat = tokio::spawn(async move {
            const HEARTBEAT_INTERVAL_S: f64 = 2.0;
            let mut tick = tokio::time::interval(Duration::from_secs(2));
            let mut tx_live = TxLiveness::new();
            let mut prev_packets: i64 = 0;
            // Previous link-lock state, so a lock/unlock event fires only on a
            // real transition (not every heartbeat). `None` until the first tick.
            let mut prev_locked: Option<bool> = None;
            // Debounced detector for the "transmitting, zero confirmed reception"
            // episode. Emits a discrete entry/clear event pair so an RCA can query
            // the episode boundaries; the instantaneous flag still rides the
            // sidecar each tick. Bounded + self-clearing, no event while healthy.
            let mut rf_detector = ados_radio::rf_unverified::RfUnverifiedDetector::new();
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
                        // Live PHY-mute readback: the TX PHY pinned at the muted
                        // not-permitted floor injects frames but radiates nothing
                        // (the RTL8812EU `set type monitor` mute). Surfaced on the
                        // sidecar so Mission Control shows a "PHY muted" badge
                        // instead of a silent dead link (Rule 28).
                        let phy_muted = ados_radio::adapter::read_tx_power(&hb_iface)
                            .await
                            .map(|dbm| dbm <= ados_radio::adapter::MUTED_TX_POWER_DBM)
                            .unwrap_or(false);
                        let stats = hb_link.lock().await.clone();
                        let wd = {
                            let mut c = hb_counters.lock().await;
                            c.phy_muted = phy_muted;
                            *c
                        };
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
                        // Received-side proof: the drone is locked only when a
                        // verified return signal was heard within the grace
                        // window; it is `rf_unverified` when injecting RF with
                        // no such signal (the transmitting-zero-reception
                        // case). Read once, before the state is derived, so the
                        // derived state, the lock/unlock events, the sidecar
                        // booleans and the episode detector all describe the
                        // same instant.
                        let now = Instant::now();
                        let tx_is_live = tx_live.tx_live();
                        let rx_proven = hb_proof.proven_within(
                            ados_radio::link_proof::RX_PROOF_GRACE,
                            now,
                            hb_proof_reference,
                        );
                        let rf_unverified = ados_radio::link_proof::is_rf_unverified(
                            tx_is_live,
                            rx_proven,
                        );
                        let state = derive_link_state(
                            tx_key_present,
                            bind_active,
                            &stats,
                            tx_is_live,
                            rx_proven,
                        );

                        // Ship the per-heartbeat link-quality samples (the
                        // downlink video radio) and a discrete lock/unlock event
                        // on a real link-state transition. Best-effort; an absent
                        // logging daemon drops these without disturbing the radio.
                        {
                            use ados_protocol::logd::{Fields, Level, Value};
                            let mut tags = Fields::new();
                            tags.insert("direction".to_string(), Value::from("downlink"));
                            tags.insert("link".to_string(), Value::from("video"));
                            hb_metrics.emit_metric("link.rssi_dbm", stats.rssi_dbm, tags.clone());
                            hb_metrics.emit_metric("link.snr_db", stats.snr_db, tags.clone());
                            hb_metrics.emit_metric(
                                "link.fec_uncorrected",
                                stats.fec_failed as f64,
                                tags.clone(),
                            );
                            // Loss + bitrate round out the link-history sample so
                            // the durable `/api/wfb/history` series is a faithful
                            // superset of the live monitor sample shape.
                            hb_metrics.emit_metric(
                                "link.loss_percent",
                                stats.loss_percent,
                                tags.clone(),
                            );
                            hb_metrics.emit_metric(
                                "link.bitrate_kbps",
                                stats.bitrate_kbps as f64,
                                tags,
                            );
                            let locked = state.is_locked();
                            if prev_locked != Some(locked) {
                                let mut detail = Fields::new();
                                detail.insert("link".to_string(), Value::from("video"));
                                detail.insert("state".to_string(), Value::from(state.as_str()));
                                if locked {
                                    hb_metrics.emit_event("link.lock", Level::Info, detail);
                                } else if prev_locked.is_some() {
                                    // Only emit unlock for a genuine drop from a
                                    // previously-locked link, not the initial
                                    // not-yet-locked state at service start.
                                    hb_metrics.emit_event("link.unlock", Level::Warn, detail);
                                }
                                prev_locked = Some(locked);
                            }
                        }

                        let bsnap = hb_bitrate.lock().await.clone();

                        // Truthful channel: read the LIVE interface channel, not
                        // the configured value. A momentary read failure keeps the
                        // last-known live value.
                        if let Some(live) = channel_from_iface(&hb_iface).await {
                            last_live_channel = live;
                        }
                        let channels = ChannelTruth {
                            actual: last_live_channel,
                            rendezvous: hb_rendezvous,
                            operating: hb_operating.load(Ordering::Relaxed) as u8,
                            locked: rx_proven,
                            rf_unverified,
                        };
                        // Discrete entry/clear event for a sustained unverified
                        // episode (the instantaneous flag already rides the
                        // sidecar above). The detector debounces a brief gap, so
                        // an event fires only on a real onset and its clear.
                        match rf_detector.observe(rf_unverified, now) {
                            ados_radio::rf_unverified::RfUnverifiedEdge::Entry => {
                                hb_events.emit(
                                    ados_radio::rf_unverified::RF_UNVERIFIED_KIND,
                                    ados_protocol::logd::Level::Warn,
                                    ados_radio::rf_unverified::rf_unverified_detail(
                                        "entry",
                                        &hb_iface,
                                        rates.tx_bytes_per_s,
                                        rates.valid_rx_packets_per_s,
                                        hb_usb_speed,
                                        ados_radio::link_proof::RX_PROOF_GRACE.as_secs(),
                                        None,
                                    ),
                                );
                            }
                            ados_radio::rf_unverified::RfUnverifiedEdge::Clear { episode_s } => {
                                hb_events.emit(
                                    ados_radio::rf_unverified::RF_UNVERIFIED_KIND,
                                    ados_protocol::logd::Level::Info,
                                    ados_radio::rf_unverified::rf_unverified_detail(
                                        "clear",
                                        &hb_iface,
                                        rates.tx_bytes_per_s,
                                        rates.valid_rx_packets_per_s,
                                        hb_usb_speed,
                                        ados_radio::link_proof::RX_PROOF_GRACE.as_secs(),
                                        Some(episode_s),
                                    ),
                                );
                            }
                            ados_radio::rf_unverified::RfUnverifiedEdge::None => {}
                        }
                        // Live regulatory status (cheap `iw reg get`), so a domain
                        // that changes under the radio surfaces remotely too.
                        let reg_status =
                            ados_radio::adapter::read_reg_status(&hb_wanted_domain).await;

                        // ── Periodic regulatory reconcile (prevention) ────────
                        // The injection PHY can re-assert its baked country as the
                        // global domain on a later monitor/bind re-entry, long
                        // after the bring-up reconcile. When the live domain drifts
                        // off the wanted value, re-assert it so the onboard WiFi is
                        // never left under a foreign domain. SAFETY: only re-assert
                        // when the wanted domain permits the rendezvous channel —
                        // the enabled set already excludes DFS / disabled channels,
                        // so membership (or an empty/unknown set) is the same gate
                        // the bring-up used; this can never cap the radio. Skipped
                        // entirely while the live domain already matches (the cheap
                        // common case), so the steady-state cost is one comparison.
                        if !reg_status.verified {
                            let channel_ok = hb_enabled.is_empty()
                                || hb_enabled.contains(&hb_rendezvous);
                            if let ados_radio::adapter::ReassertOutcome::Reasserted {
                                from,
                                to,
                                ..
                            } = ados_radio::adapter::reconcile_reg_domain(
                                &hb_wanted_domain,
                                hb_rendezvous,
                                channel_ok,
                            )
                            .await
                            {
                                hb_events.emit(
                                    ados_radio::reg_reassert::REG_REASSERT_KIND,
                                    ados_radio::reg_reassert::REG_REASSERT_SEVERITY,
                                    ados_radio::reg_reassert::reg_reassert_detail(
                                        &hb_iface,
                                        from.as_deref(),
                                        &to,
                                        hb_rendezvous,
                                        true,
                                    ),
                                );
                            }
                        }

                        let reg = RegSnapshot {
                            domain: reg_status.domain,
                            verified: reg_status.verified,
                            enabled_channels: hb_enabled.clone(),
                            posture: hb_reg_posture.clone(),
                        };

                        // Build the full body once, write the sidecar (the live
                        // fallback the REST route reads), and ship the SAME body
                        // to the logging store as a single full-snapshot event
                        // (the durable read source). Best-effort: an absent
                        // logging daemon drops the event without disturbing the
                        // radio loop.
                        let body = build_stats_value(
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
                        ados_radio::paths::write_sidecar(
                            &ados_radio::paths::run_path("wfb-stats.json"),
                            &body,
                        )
                        .ok();
                        hb_metrics.emit_event(
                            "link.wfb_status",
                            ados_protocol::logd::Level::Info,
                            json_object_to_fields(&body),
                        );
                    }
                    _ = hb_cancel.notified() => break,
                }
            }
        });

        let tx_cancel = task_cancel.clone();
        let tx_iface = iface_str.clone();
        let tx_counters = counters.clone();
        // Hand the watchdog the shared process handle, not a captured PID: the
        // data plane is respawned (new PID) on every FEC/MCS/tier/adaptive
        // change, so the watchdog must resolve the live data-tx PID each poll to
        // keep its ingress (`rchar`) signal pinned to the running process.
        let tx_proc = proc.clone();
        let mut watchdog1 = tokio::spawn(async move {
            tx_health_watchdog(&tx_iface, tx_proc, tx_counters, tx_cancel).await
        });

        let recvq_cancel = task_cancel.clone();
        let recvq_counters = counters.clone();
        let mut watchdog2 =
            tokio::spawn(async move { video_recvq_watchdog(recvq_counters, recvq_cancel).await });

        // Auxiliary application-stream liveness watchdog. Idles while the aux pair
        // is closed (the safe-by-default boot state) and, when a plugin has opened
        // the stream, applies the same delta-counter contract the data plane uses
        // — a flat ingress counter on a live aux transmitter is a silent stall.
        // It owns its OWN recovery (restart only the aux pair in place), so it
        // never trips the whole-group respawn select; it ends only when cancelled,
        // and is aborted alongside the other siblings on respawn/shutdown.
        let aux_wd_cancel = task_cancel.clone();
        let aux_wd_proc = proc.clone();
        // Not `&mut`-selected in the run loop (it owns its own recovery and ends
        // only on cancel), so it is only ever aborted, never polled by the select.
        let aux_watchdog =
            tokio::spawn(async move { aux_liveness_watchdog(aux_wd_proc, aux_wd_cancel).await });

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

        // Radio command socket: an operator (the REST layer) can trigger a
        // coordinated channel hop on demand. Validated requests flow over this
        // bounded mpsc to the hop supervisor's select loop, which runs the
        // paired/peer/bind checks and drives the EXISTING announce path so the
        // GS follows. Spawned per bring-up alongside the sibling tasks (aborted +
        // re-served on every respawn). The receiver is handed to the hop
        // supervisor below; when auto-hop is fully disabled (the else branch runs
        // proof_only_listener) no receiver drains it, so the socket reports
        // `unavailable` for hop requests rather than hanging.
        let (manual_hop_tx, manual_hop_rx) =
            tokio::sync::mpsc::channel::<ados_radio::radio_cmd::ManualHopRequest>(8);
        let radio_cmd_state = ados_radio::radio_cmd::CmdState {
            hop_tx: manual_hop_tx,
            operating_channel: operating_channel.clone(),
        };
        let radio_cmd_cancel = task_cancel.clone();
        let radio_cmd_sock_path = ados_radio::paths::run_path("radio-cmd.sock");
        let radio_cmd_server = tokio::spawn(async move {
            tokio::select! {
                r = ados_radio::radio_cmd::serve(
                    radio_cmd_state,
                    Path::new(&radio_cmd_sock_path),
                ) => {
                    if let Err(e) = r {
                        tracing::warn!(error = %e, "radio_command_socket_serve_ended");
                    }
                }
                _ = radio_cmd_cancel.notified() => {}
            }
        });

        // Auxiliary-stream command socket: a plugin (via the plugin host) opens or
        // closes the additive aux transmit/receive pair through this socket. Holds
        // the SAME process handle the watchdogs + operator socket use, and the boot
        // config (the source of the effective aux ports/FEC/MCS an open applies).
        // SAFE-BY-DEFAULT: nothing starts here at bring-up — the pair exists only
        // between an explicit open and its close. Spawned + aborted per bring-up
        // like the sibling sockets.
        let aux_cmd_state = AuxCmdState {
            proc: proc.clone(),
            cfg: Arc::new(cfg.clone()),
        };
        let aux_cmd_cancel = task_cancel.clone();
        let aux_cmd_sock_path = ados_radio::paths::run_path("radio-aux.sock");
        let aux_cmd_server = tokio::spawn(async move {
            tokio::select! {
                r = aux_cmd::serve(aux_cmd_state, Path::new(&aux_cmd_sock_path)) => {
                    if let Err(e) = r {
                        tracing::warn!(error = %e, "aux_command_socket_serve_ended");
                    }
                }
                _ = aux_cmd_cancel.notified() => {}
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
                    manual_hop_rx,
                    hop_cancel,
                )
                .await;
            } else {
                // Auto-hop fully disabled: drain (and reject) the manual-hop
                // channel so the command socket's `try_send` reports `unavailable`
                // promptly instead of filling the buffer. Dropping the receiver
                // here closes the channel, which the socket reads as unavailable.
                drop(manual_hop_rx);
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
        // Inner select loop: most fires fall through (break) to the full kill +
        // respawn below, but a runtime PHY-mute is recovered IN PLACE (re-cycle
        // monitor + channel + txpower) without killing wfb_tx — respawning the
        // process can never un-mute a driver/PHY-level mute, so the old kill path
        // looped forever. On a successful in-place recovery we re-arm the TX
        // watchdog and `continue` to keep watching the live radio group.
        loop {
            // `biased` + the shutdown arm FIRST: a latched SIGTERM must win the
            // poll even when a watchdog / data-tx-exit task is simultaneously ready,
            // so a real shutdown is never mistaken for a respawn trigger.
            tokio::select! {
                biased;
                _ = wait_for_shutdown_flag(&mut shutdown) => {
                    // Clean shutdown: stop the tasks, the radio group, then restore
                    // the adapter to managed mode so it isn't left stuck in monitor.
                    cancel_bridge.abort();
                    heartbeat.abort();
                    watchdog1.abort();
                    watchdog2.abort();
                    aux_watchdog.abort();
                    data_tx_exit.abort();
                    bitrate_ctrl.abort();
                    cmd_server.abort();
                    radio_cmd_server.abort();
                    aux_cmd_server.abort();
                    hop.abort();
                    beacon.abort();
                    proc.lock().await.kill_all().await;
                    ados_radio::adapter::set_managed_mode(iface).await;
                    tracing::info!("wfb_service_stopping");
                    return;
                }
                result = &mut watchdog1 => {
                    // A finished watchdog could win this poll in the same instant a
                    // SIGTERM arrives; the latched-watch re-check after the select
                    // (and the top-of-respawn-loop gate) catches that ordering so we
                    // never respawn into a stopping service.
                    match result {
                        Ok(WatchdogFired::PhyMuted) => {
                            tracing::warn!(iface, "watchdog_phy_muted: attempting in-place PHY recovery");
                            if ensure_radiating(iface, cfg.channel, cfg.tx_power_dbm, unrestricted)
                                .await
                                .is_some()
                            {
                                tracing::info!(iface, "watchdog_phy_recovered_in_place");
                                let tx_cancel = task_cancel.clone();
                                let tx_iface = iface_str.clone();
                                let tx_counters = counters.clone();
                                let tx_proc = proc.clone();
                                watchdog1 = tokio::spawn(async move {
                                    tx_health_watchdog(&tx_iface, tx_proc, tx_counters, tx_cancel).await
                                });
                                continue;
                            }
                            tracing::warn!(iface, "watchdog_phy_recovery_failed: killing wfb_tx");
                        }
                        Ok(WatchdogFired::TxStalled | WatchdogFired::RecvqBacklog) => {
                            tracing::warn!("watchdog_fired_killing_wfb_tx");
                        }
                        _ => {}
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
            }
            break;
        }

        // Latched shutdown re-check: a watchdog / exit-poll arm can legitimately
        // win the select in the same instant SIGTERM flips the watch. Honor the
        // shutdown here rather than falling into the respawn path below, then run
        // the same clean teardown the shutdown arm runs.
        if *shutdown.borrow() {
            cancel_bridge.abort();
            heartbeat.abort();
            watchdog1.abort();
            watchdog2.abort();
            aux_watchdog.abort();
            data_tx_exit.abort();
            bitrate_ctrl.abort();
            cmd_server.abort();
            radio_cmd_server.abort();
            aux_cmd_server.abort();
            hop.abort();
            beacon.abort();
            proc.lock().await.kill_all().await;
            ados_radio::adapter::set_managed_mode(iface).await;
            tracing::info!("wfb_service_stopping");
            return;
        }

        // A task exited (watchdog fired / hop ended / data-tx self-crashed) —
        // abort the siblings so they don't accumulate, kill the whole radio
        // group, and respawn.
        cancel_bridge.abort();
        heartbeat.abort();
        watchdog1.abort();
        watchdog2.abort();
        aux_watchdog.abort();
        data_tx_exit.abort();
        bitrate_ctrl.abort();
        cmd_server.abort();
        radio_cmd_server.abort();
        aux_cmd_server.abort();
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
                posture: reg_posture.clone(),
            },
            effective_tx_dbm,
            Some(&adapter_info),
            &LinkStats::default(),
            cfg,
            restart_count.load(Ordering::Relaxed),
            &wd,
            &TxRates::default(),
            &bitrate_snapshot.lock().await.clone(),
            Some(&metrics),
        );
        tokio::select! {
            biased;
            _ = wait_for_shutdown_flag(&mut shutdown) => return,
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
    }
}

/// Wait until the latched shutdown watch flips to `true`. Returns immediately if
/// it is already set (the latch never loses an edge) and on a closed channel
/// (the sender dropped — treat as shutdown). The single owner of the `&mut`
/// receiver for the run-loop's own shutdown checks; worker tasks get their wake
/// via the per-bring-up `Notify` bridge.
async fn wait_for_shutdown_flag(shutdown: &mut watch::Receiver<bool>) {
    // `wait_for` resolves as soon as the predicate holds — including on the
    // current value — and on sender-drop it returns `Err`, which we also treat as
    // a shutdown signal so a vanished sender never strands the loop.
    let _ = shutdown.wait_for(|s| *s).await;
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
mod shutdown_tests {
    use super::*;

    /// A latched shutdown watch that is already `true` resolves
    /// `wait_for_shutdown_flag` immediately — the latch never loses the edge, so
    /// a SIGTERM that flipped the watch while a select arm was busy is still seen
    /// on the next poll.
    #[tokio::test]
    async fn shutdown_flag_already_set_resolves_immediately() {
        let (tx, rx) = watch::channel(false);
        tx.send(true).unwrap();
        let mut rx = rx;
        // No timeout needed: if the latch were lost this would hang and the test
        // harness would catch it, but on a correct latch it returns at once.
        wait_for_shutdown_flag(&mut rx).await;
    }

    /// With `biased;` and the shutdown arm FIRST, a latched shutdown wins the
    /// poll even when a competing arm (a finished "watchdog" task) is also ready
    /// in the same instant — so a real shutdown is never mistaken for a respawn
    /// trigger.
    #[tokio::test]
    async fn shutdown_arm_wins_over_a_ready_competitor() {
        let (tx, rx) = watch::channel(false);
        tx.send(true).unwrap();
        let mut rx = rx;
        // A competing future that is immediately ready (stands in for a watchdog
        // task that finished and would otherwise route into the respawn path).
        let competitor = std::future::ready(());

        #[derive(Debug, PartialEq)]
        enum Won {
            Shutdown,
            Competitor,
        }
        let won = tokio::select! {
            biased;
            _ = wait_for_shutdown_flag(&mut rx) => Won::Shutdown,
            _ = competitor => Won::Competitor,
        };
        assert_eq!(
            won,
            Won::Shutdown,
            "the biased shutdown-first arm must win over a ready competitor"
        );
    }

    /// A dropped sender (no more shutdown signal possible) also resolves the
    /// wait, so a vanished signaller never strands the run loop forever.
    #[tokio::test]
    async fn shutdown_flag_resolves_on_sender_drop() {
        let (tx, rx) = watch::channel(false);
        let mut rx = rx;
        drop(tx);
        wait_for_shutdown_flag(&mut rx).await;
    }
}
