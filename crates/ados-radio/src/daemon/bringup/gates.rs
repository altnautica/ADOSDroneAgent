//! The two regulatory-gate stages a radio-group bring-up passes before it
//! touches the channel: the domain set + verify (stage 1, before monitor mode)
//! and the rendezvous-channel readiness assertion (stage 2, once the wiphy
//! exists). A strict-mode failure writes the `reg_blocked` sidecar and parks.

use std::collections::BTreeSet;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::adapter;
use crate::link_quality::LinkStats;
use crate::reg_event::{
    is_eeprom_override, reg_gate_detail, RegGateResult, RegGateStage, REG_GATE_KIND,
};
use crate::reg_gate::{decide_reg_gate, RegGateDecision, BRINGUP_RETRY_SECS, STATE_REG_BLOCKED};
use crate::watchdog::WatchdogCounters;

use super::Park;
use crate::daemon::service::Service;
use crate::daemon::sidecar::{write_stats_sidecar, ChannelTruth, RegSnapshot};
use crate::daemon::txrate::TxRates;

/// Regulatory gate, stage 1: set + VERIFY the domain BEFORE the adapter is
/// brought up in monitor mode. The kernel maps the permitted channel set and the
/// per-channel TX-power ceiling at monitor-mode bring-up, so a domain set
/// afterwards is too late and leaves the home channel (149, 5745 MHz) capped.
/// This is a global per-phy call, so it needs no interface and cannot disturb
/// the operator's management link. On verify failure under the strict (default)
/// gate the radio parks in `reg_blocked` and retries rather than radiating on a
/// forbidden band; the lab escape hatch (`reg_gate_strict: false`) proceeds
/// best-effort. A None config value falls back to the safe default.
pub(super) async fn domain_gate(svc: &Service<'_>, rendezvous_ch: u8) -> Result<(), Park> {
    let cfg = svc.cfg;
    // Unrestricted → the posture sets an EMPTY domain (set_reg_domain
    // no-ops on ""), so the bring-up does not force a country onto the
    // injection PHY; the global cfg80211 reconciler in the supervisor
    // keeps the GLOBAL domain sane independently. Region → the pinned
    // region code.
    let domain = svc.posture.domain_to_set.as_str();
    let reg_result = adapter::set_reg_domain(domain).await;
    let eeprom_override = reg_result
        .as_ref()
        .err()
        .map(is_eeprom_override)
        .unwrap_or(false);
    // Under the unrestricted posture the gate never blocks: the posture's
    // strictness is off regardless of the raw `reg_gate_strict` flag.
    match decide_reg_gate(&reg_result, svc.posture.strict) {
        RegGateDecision::Proceed => {
            svc.events.emit(
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
            svc.events.emit(
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
            let status = adapter::read_reg_status(domain).await;
            svc.events.emit(
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
                    posture: svc.reg_posture.clone(),
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
            return Err(Park(Duration::from_secs(BRINGUP_RETRY_SECS)));
        }
    }
    Ok(())
}

/// Regulatory gate, stage 2: assert the rendezvous channel is in the domain's
/// enabled set and is non-DFS, NOW that the interface (and its wiphy) exist,
/// BEFORE setting the channel / TX power / spawning wfb_tx. This catches a
/// domain/channel mismatch at preflight instead of as a silent power-cap on a
/// fallback frequency. An empty enabled set means the wiphy list was unreadable,
/// which the assertion treats as "could not determine" and passes, so a board
/// with an unreadable channel list still comes up. On a strict-mode failure the
/// radio parks in `reg_blocked` and retries; no wfb_tx is ever spawned on a bad
/// channel.
pub(super) async fn channel_gate(
    svc: &Service<'_>,
    rendezvous_ch: u8,
    gate_enabled: &BTreeSet<u8>,
    gate_dfs: &BTreeSet<u8>,
) -> Result<(), Park> {
    let cfg = svc.cfg;
    let domain = svc.posture.domain_to_set.as_str();
    // Unrestricted → skip the channel-enabled / non-DFS assertion entirely
    // (the operator owns local compliance); the rendezvous channel is used
    // as-is. Region → today's strict channel readiness check.
    let ready = if svc.unrestricted {
        Ok(())
    } else {
        adapter::assert_reg_ready(rendezvous_ch, gate_enabled, gate_dfs, cfg.dfs_allowed)
    };
    match decide_reg_gate(&ready, svc.posture.strict) {
        RegGateDecision::Proceed => {
            svc.events.emit(
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
            let status = adapter::read_reg_status(domain).await;
            svc.events.emit(
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
            let status = adapter::read_reg_status(domain).await;
            svc.events.emit(
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
                    posture: svc.reg_posture.clone(),
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
            return Err(Park(Duration::from_secs(BRINGUP_RETRY_SECS)));
        }
    }
    Ok(())
}
