//! One radio-group bring-up: the key guard, the regulatory gates, adapter
//! selection, monitor mode + channel, the regulatory re-assert, the TX-power
//! clamp and the process-group spawn. [`bring_up_group`] returns the live
//! [`GroupCtx`] the run loop hands to the group's tasks, or a [`Park`] telling
//! the loop how long to wait before the next attempt.

mod gates;
pub(crate) mod phy;

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::adapter;
use crate::config::WfbConfig;
use crate::hop::{pair_key_for, respawn_channel};
use crate::link_proof::RxProof;
use crate::link_quality::LinkStats;
use crate::paths::{load_shared_key, SharedKey, WFB_TX_KEY};
use crate::process::RadioProcesses;
use crate::reg_gate::BRINGUP_RETRY_SECS;
use crate::watchdog::{new_counters, CounterHandle, WatchdogCounters};

use super::service::Service;
use super::sidecar::{
    write_adapters_sidecar, write_stats_sidecar, AdapterInfo, ChannelTruth, RegSnapshot,
};
use super::txrate::TxRates;
use super::KEY_WAIT_INTERVAL;
use phy::{ensure_monitor_and_channel, ensure_radiating};

/// A bring-up that did not reach a running group: wait this long (or until
/// shutdown) and start the next attempt from the top of the run loop.
pub(super) struct Park(pub(super) Duration);

/// Everything a running radio group's tasks share, produced by one successful
/// [`bring_up_group`].
pub(super) struct GroupCtx {
    pub(super) iface: String,
    pub(super) rendezvous_ch: u8,
    pub(super) bringup_ch: u8,
    pub(super) effective_tx_dbm: i8,
    pub(super) pair_key: Option<[u8; 32]>,
    pub(super) enabled_channels: Arc<BTreeSet<u8>>,
    pub(super) enabled_channels_vec: Vec<u8>,
    pub(super) rx_proof: RxProof,
    pub(super) proof_reference: Instant,
    pub(super) wanted_domain: String,
    pub(super) link: Arc<tokio::sync::Mutex<LinkStats>>,
    pub(super) counters: CounterHandle,
    pub(super) spawn_cfg: WfbConfig,
    pub(super) proc: Arc<tokio::sync::Mutex<RadioProcesses>>,
    pub(super) adapter_info: AdapterInfo,
    pub(super) reg_status: adapter::RegStatus,
}

fn park_retry() -> Park {
    Park(Duration::from_secs(BRINGUP_RETRY_SECS))
}

/// Bring one radio group up, or park. `retained_trio` is the data-plane trio
/// the previous group was running (`None` until a group ran).
pub(super) async fn bring_up_group(
    svc: &Service<'_>,
    retained_trio: Option<(u8, u8, u8)>,
) -> Result<GroupCtx, Park> {
    let cfg = svc.cfg;
    // ── Key guard — block while unpaired ─────────────────────────────
    if !Path::new(WFB_TX_KEY).exists() {
        tracing::info!(key = WFB_TX_KEY, "wfb_blocked_unpaired");
        write_stats_sidecar(
            "unpaired",
            &ChannelTruth::configured(cfg.rendezvous_channel()),
            &RegSnapshot {
                posture: svc.reg_posture.clone(),
                ..RegSnapshot::default()
            },
            svc.tx_power.requested(),
            None,
            &LinkStats::default(),
            cfg,
            svc.restart_count.load(Ordering::Relaxed),
            &WatchdogCounters::default(),
            &TxRates::default(),
            &svc.bitrate_snapshot.lock().await.clone(),
            Some(&svc.metrics),
        );
        return Err(Park(KEY_WAIT_INTERVAL));
    }

    let rendezvous_ch = cfg.rendezvous_channel();
    gates::domain_gate(svc, rendezvous_ch).await?;

    // ── Adapter selection ─────────────────────────────────────────────
    // Detect and verify every adapter (the full list feeds the adapters
    // sidecar), resolve the configured roles, and transmit on the adapter
    // the roles select. The outcome carries the scan counts for the loud
    // no-injection diagnostic. The air side has no mesh carrier.
    let outcome = adapter::detect_all(&cfg.interface).await;
    // Publish the full detected list so the GCS panel + the permanent-Python
    // seam see the scan verdict regardless of whether a radio was found.
    write_adapters_sidecar(&outcome.scanned);
    let roles_cfg = crate::config::RadioAdaptersConfig::load_from(Path::new(super::CONFIG_YAML));
    let plan = adapter::AdapterPlan::new(adapter::resolve_roles(
        &outcome.verified(),
        &roles_cfg,
        false,
    ));
    let Some(adapter) = plan.map(|p| p.tx().adapter.clone()) else {
        // No RTL injection-capable adapter could be proven. Fail LOUDLY with
        // the diagnostic counts (total detected + compatible-and-monitor),
        // and keep the sidecar's stranded-radio signal — adapter chipset
        // null, adapter_injection_ok false (a MEASURED verdict: the scan
        // ran and proved none) — so the panel shows the warning rather
        // than a false "connecting" with zero injected frames. The USB
        // link-health fields stay null: nothing enumerated.
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
                posture: svc.reg_posture.clone(),
                ..RegSnapshot::default()
            },
            svc.tx_power.requested(),
            // The scanned-and-none-found record: the injection verdict is
            // a measured false, not the null an unscanned rig reports.
            Some(&AdapterInfo::none_found()),
            &LinkStats::default(),
            cfg,
            svc.restart_count.load(Ordering::Relaxed),
            &WatchdogCounters::default(),
            &TxRates::default(),
            &svc.bitrate_snapshot.lock().await.clone(),
            Some(&svc.metrics),
        );
        return Err(park_retry());
    };

    tracing::info!(
        iface = %adapter.ifname,
        chipset = %adapter.chipset,
        verified = adapter.verified,
        "adapter_selected"
    );

    let iface = &adapter.ifname;
    let gate_enabled: BTreeSet<u8> = adapter::enabled_channels(iface).await;
    let gate_dfs: BTreeSet<u8> = adapter::dfs_channels(iface).await;
    gates::channel_gate(svc, rendezvous_ch, &gate_enabled, &gate_dfs).await?;
    // The channel this bring-up tunes: the rendezvous home on a fresh start,
    // or the committed hop channel while the peer was heard recently, so a
    // respawn after a hop never splits the pair.
    let bringup_ch = respawn_channel(&*svc.hop_state.lock().await, Instant::now());
    // Re-assert monitor mode and land on the channel before bring-up
    // proceeds. A self-managed injection PHY (the RTL family) — or a
    // concurrent regulatory-domain set on the same wiphy — can revert the
    // vif to managed between adapter selection and here; a managed interface
    // rejects `iw set channel` with EBUSY. Spawning wfb_tx on a managed /
    // mis-tuned interface advances tx_bytes while radiating nothing a ground
    // station can decode (advancing tx_bytes with zero usable RF), so the
    // channel must land — verified — before TX starts. On total failure,
    // re-enter the selection loop rather than start a dead TX.
    if !ensure_monitor_and_channel(iface, bringup_ch).await {
        tracing::error!(
            iface,
            channel = bringup_ch,
            note =
                "monitor mode + channel never landed (EBUSY); not starting TX, retrying bring-up",
            "wfb_channel_set_unrecovered"
        );
        return Err(park_retry());
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
        use crate::adapter::ReassertOutcome;
        use crate::reg_reassert::{reg_reassert_detail, REG_REASSERT_KIND, REG_REASSERT_SEVERITY};
        // Keep the GLOBAL domain sane to protect the onboard management WiFi
        // (this is the prevention layer, orthogonal to the WFB-TX gate above).
        // Under the unrestricted posture this still re-asserts the configured
        // fallback domain (never `00`), so a self-managed baked country can
        // never linger as the effective global and strand the onboard WiFi.
        let wanted = svc.region_domain.as_str();
        // The wanted domain must permit the rendezvous channel before we
        // force it. The bring-up gate read this set for THIS adapter already;
        // reuse it (no second `iw phy channels` call).
        let channel_ok =
            adapter::assert_reg_ready(rendezvous_ch, &gate_enabled, &gate_dfs, cfg.dfs_allowed)
                .is_ok();
        match adapter::reconcile_reg_domain(wanted, rendezvous_ch, channel_ok).await {
            ReassertOutcome::Reasserted { from, to, .. } => {
                svc.events.emit(
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
    // out the adapter. Ramps up from the configured floor on rejection, never
    // past the configured ceiling. The power-budget clamp is NEVER lifted by
    // the unrestricted posture — only the muted-readback handling differs
    // (surface vs abort, see set_tx_power_modal).
    let effective_tx_dbm = match ensure_radiating(
        iface,
        bringup_ch,
        svc.tx_power.requested(),
        svc.tx_power.max_dbm(),
        svc.unrestricted,
    )
    .await
    {
        Some(dbm) => {
            svc.tx_power.record_applied(Some(dbm));
            dbm
        }
        None => {
            // No in-budget TX power was accepted, or the PHY stayed pinned at
            // the muted not-permitted floor through every recovery attempt.
            // Starting wfb_tx now either injects into a dead PHY (every
            // sendmsg returns ENOBUFS) or radiates at the driver default.
            // Park and re-enter bring-up instead.
            tracing::error!(
                iface,
                channel = bringup_ch,
                note = "TX power not applied in budget or PHY muted after recovery; not starting TX, retrying bring-up",
                "wfb_phy_muted_unrecovered"
            );
            return Err(park_retry());
        }
    };

    // ── Load pair key for HMAC derivation ────────────────────────────
    // An unbound node (no key file) runs on the cold-start constant; a key
    // file that exists but cannot be read leaves the control plane with no
    // key at all, so nothing is authenticated rather than everything being
    // authenticated under a constant anyone can compute.
    let shared_key = load_shared_key();
    if let SharedKey::Unavailable(reason) = &shared_key {
        tracing::error!(%reason, "hop_pair_key_unavailable: hop and presence authentication refused");
    }
    let pair_key = pair_key_for(&shared_key);

    // ── Regulatory-enabled channel set, for the hop target filter ─────
    // Channels this adapter's reg domain forbids fail `iw set channel` with
    // -22 and split the pair onto divergent frequencies; the hop loop
    // intersects its candidates with this set. Empty = "could not
    // determine" → do not restrict. Reuse the set the regulatory gate
    // already read for this adapter bring-up (no second `iw` call). Under the
    // unrestricted posture the hop filter is disabled (empty set) so the hop
    // loop may target any in-band channel — the operator owns compliance.
    let enabled_channels = Arc::new(if svc.unrestricted {
        BTreeSet::new()
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
    let rx_proof = RxProof::new();
    let proof_reference = Instant::now();
    // Runtime operating channel (process-lifetime, hoisted above the respawn
    // loop): the channel this bring-up landed on. The hop supervisor updates
    // it on a committed move; the heartbeat reads it for `operating_channel`.
    svc.operating_channel
        .store(bringup_ch as u64, Ordering::Relaxed);
    // The wanted regulatory domain, resolved once (constant for this bring-up)
    // so the heartbeat's cheap live read can report verified vs unverified.
    // Region → the pinned region; unrestricted → the configured fallback (the
    // global reconciler keeps that sane, so `verified` still means something).
    let wanted_domain = svc.region_domain.clone();

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
    // Bring the data plane up on the trio the previous group was running, so
    // a watchdog kill or a data-tx crash never reverts a pinned manual tier,
    // preset or adaptive step to the boot config.
    let spawn_cfg = match retained_trio {
        Some((fec_k, fec_n, mcs)) => crate::process::data_cfg_from_retained(cfg, fec_k, fec_n, mcs),
        None => cfg.clone(),
    };
    let proc = match RadioProcesses::spawn(iface, &spawn_cfg, key_path, link.clone()).await {
        Ok(p) => Arc::new(tokio::sync::Mutex::new(p)),
        Err(e) => {
            tracing::warn!(error = %e, "wfb_spawn_failed");
            return Err(park_retry());
        }
    };
    let pid = { proc.lock().await.data_tx_pid().unwrap_or(0) };
    let adapter_info = AdapterInfo {
        interface: iface.clone(),
        chipset: adapter.chipset.clone(),
        injection_ok: adapter.verified,
        usb_speed_mbps: adapter.usb_speed_mbps,
        usb_degraded: adapter.usb_degraded,
    };
    // Live regulatory status for the sidecar (the wanted domain is constant
    // for this bring-up; the live read is a cheap `iw reg get`). Re-read by
    // the heartbeat each tick so a domain that changes under the radio
    // surfaces, but seeded here so the first `connecting` sidecar is truthful.
    let reg_status = adapter::read_reg_status(&wanted_domain).await;
    let reg_snapshot = RegSnapshot {
        domain: reg_status.domain.clone(),
        verified: reg_status.verified,
        enabled_channels: enabled_channels_vec.clone(),
        posture: svc.reg_posture.clone(),
    };
    write_stats_sidecar(
        "connecting",
        &ChannelTruth::configured(rendezvous_ch),
        &reg_snapshot,
        effective_tx_dbm,
        Some(&adapter_info),
        &LinkStats::default(),
        cfg,
        svc.restart_count.load(Ordering::Relaxed),
        &WatchdogCounters::default(),
        &TxRates::default(),
        &svc.bitrate_snapshot.lock().await.clone(),
        Some(&svc.metrics),
    );
    tracing::info!(iface, channel = rendezvous_ch, pid, "wfb_service_ready");

    Ok(GroupCtx {
        iface: iface.clone(),
        rendezvous_ch,
        bringup_ch,
        effective_tx_dbm,
        pair_key,
        enabled_channels,
        enabled_channels_vec,
        rx_proof,
        proof_reference,
        wanted_domain,
        link,
        counters,
        spawn_cfg,
        proc,
        adapter_info,
        reg_status,
    })
}
