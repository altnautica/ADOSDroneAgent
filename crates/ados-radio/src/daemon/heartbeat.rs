//! The 2 s sidecar heartbeat of a running radio group.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ados_protocol::shutdown::Shutdown;
use tokio::task::JoinHandle;

use crate::link_state::derive_link_state;
use crate::paths::{read_bind_sentinel_active, WFB_TX_KEY};
use crate::process::TxPlaneCounters;

use super::bringup::GroupCtx;
use super::service::Service;
use super::sidecar::{build_stats_value, json_object_to_fields, ChannelTruth, RegSnapshot};
use super::status_emit;
use super::txrate::{TxLiveness, TxRates};
use super::REG_RECONCILE_INTERVAL;

/// 2 s sidecar heartbeat — reads the live link stats + the tx_bytes
/// liveness + the bind sentinel, derives the link state, and keeps
/// wfb-stats.json fresh so the REST handler never marks it stale
/// (mtime > 10 s).
pub(super) fn spawn_heartbeat(
    svc: &Service<'_>,
    g: &GroupCtx,
    hb_cancel: Shutdown,
    data_stats: &Arc<TxPlaneCounters>,
) -> JoinHandle<()> {
    let hb_cfg = svc.cfg.clone();
    let hb_adapter = g.adapter_info.clone();
    let hb_rendezvous = g.rendezvous_ch;
    let hb_link = g.link.clone();
    let hb_iface = g.iface.clone();
    let hb_data_stats = data_stats.clone();
    let hb_restart = svc.restart_count.clone();
    let hb_counters = g.counters.clone();
    let hb_bitrate = svc.bitrate_snapshot.clone();
    let hb_proof = g.rx_proof.clone();
    let hb_proof_reference = g.proof_reference;
    let hb_operating = svc.operating_channel.clone();
    let hb_wanted_domain = g.wanted_domain.clone();
    let hb_enabled = g.enabled_channels_vec.clone();
    let hb_reg_posture = svc.reg_posture.clone();
    let hb_events = svc.events.clone();
    let hb_metrics = svc.metrics.clone();
    let hb_usb_speed = g.adapter_info.usb_speed_mbps;
    let hb_tx_power = svc.tx_power.clone();
    let hb_reg_seed = g.reg_status.clone();
    let bringup_ch = g.bringup_ch;
    let effective_tx_dbm = g.effective_tx_dbm;
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        let mut tx_live = TxLiveness::new();
        // The cached live regulatory status: seeded by the bring-up read and
        // refreshed with `iw reg get` at most once per REG_RECONCILE_INTERVAL
        // (the reconcile, when the domain drifted, runs on the same
        // cadence), so a 2 s tick forks one `iw` for the interface info and
        // nothing else.
        let mut reg_status = hb_reg_seed;
        let mut last_reg_read = Instant::now();
        // Previous link-lock state, so a lock/unlock event fires only on a
        // real transition (not every heartbeat). `None` until the first tick.
        let mut prev_locked: Option<bool> = None;
        // Debounced detector for the "transmitting, zero confirmed reception"
        // episode. Emits a discrete entry/clear event pair so an RCA can query
        // the episode boundaries; the instantaneous flag still rides the
        // sidecar each tick. Bounded + self-clearing, no event while healthy.
        let mut rf_detector = crate::rf_unverified::RfUnverifiedDetector::new();
        // The `link.wfb_status` store event goes out on a change of a key
        // field or every 10 s, not on every sidecar rewrite.
        let mut status_gate = status_emit::StatusEmitGate::new();
        // Last successfully-read live channel; if `iw info` momentarily fails
        // we keep reporting the last-known live value rather than the
        // configured one (a transient read error is not a channel change).
        let mut last_live_channel = bringup_ch;
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    tx_live.observe(hb_data_stats.totals().bytes_injected);
                    // The plane stderr logs live on the RAM-backed /run and a
                    // congested transmitter writes one line a second for as
                    // long as it runs; keep each under its cap.
                    crate::process::cap_plane_logs().await;
                    // One `iw dev <if> info` per tick, parsed into the live
                    // channel and the TX-power readback. The readback drives
                    // the PHY-mute flag (the TX PHY pinned at the muted
                    // not-permitted floor injects frames but radiates nothing)
                    // and is published as the measured TX power.
                    let info = crate::adapter::read_iface_info(&hb_iface).await;
                    let readback = info.and_then(|i| i.txpower_dbm);
                    let phy_muted = readback
                        .map(|dbm| dbm <= crate::adapter::MUTED_TX_POWER_DBM)
                        .unwrap_or(false);
                    let stats = hb_link.lock().await.clone();
                    let wd = {
                        let mut c = hb_counters.lock().await;
                        c.phy_muted = phy_muted;
                        c.tx_power_readback_dbm = readback;
                        *c
                    };
                    // Uplink valid-decode rate: the latest per-interval
                    // decode count from the rx-control stats stream. 0 on a
                    // drone hearing no uplink.
                    let rates = TxRates::sample(&tx_live, &stats);
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
                        crate::link_proof::RX_PROOF_GRACE,
                        now,
                        hb_proof_reference,
                    );
                    let rf_unverified = crate::link_proof::is_rf_unverified(
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

                    // Ship the per-heartbeat link-quality samples and a
                    // discrete lock/unlock event on a real link-state
                    // transition. Best-effort; an absent logging daemon drops
                    // these without disturbing the radio.
                    //
                    // The samples describe the UPLINK this drone receives, not
                    // its own video downlink: a single radio in monitor mode
                    // cannot capture its own injected frames, so the only link
                    // this end can measure is the one arriving. The downlink's
                    // loss is measured by the receiving station and arrives as
                    // the peer sample the bitrate ladder steps on.
                    {
                        use ados_protocol::logd::{Fields, Level, Value};
                        let mut tags = Fields::new();
                        tags.insert("direction".to_string(), Value::from("uplink"));
                        tags.insert("link".to_string(), Value::from("control"));
                        // Decoded-packet count is the sample DENOMINATOR and is
                        // always emitted: it is an honest zero, and it is what
                        // makes the absence of the rows below attributable to
                        // "nothing was decoded" rather than to a dead emitter.
                        hb_metrics.emit_metric(
                            "link.packets_received",
                            stats.packets_received as f64,
                            tags.clone(),
                        );
                        // Everything past here requires a real decode. The
                        // sidecar already nulls these when unmeasured; the
                        // durable series skipped that gate, so a radio that had
                        // heard nothing wrote -100 dBm / 0 dB SNR / 0% loss into
                        // `/api/wfb/history` every 2 s and an operator reading
                        // the series back saw a weak-but-lossless link that had
                        // never existed. Absent is the honest record; a
                        // fabricated row is not recoverable downstream.
                        if stats.is_measured() {
                            hb_metrics.emit_metric(
                                "link.rssi_dbm",
                                stats.rssi_dbm,
                                tags.clone(),
                            );
                            hb_metrics.emit_metric("link.snr_db", stats.snr_db, tags.clone());
                            hb_metrics.emit_metric(
                                "link.fec_uncorrected",
                                stats.fec_failed as f64,
                                tags.clone(),
                            );
                            // Loss + bitrate round out the link-history sample
                            // so the durable `/api/wfb/history` series is a
                            // faithful superset of the live monitor sample shape.
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
                        }
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

                    // Truthful channel: the LIVE interface channel from this
                    // tick's info read, not the configured value. A momentary
                    // read failure keeps the last-known live value.
                    if let Some(live) = info.and_then(|i| i.channel) {
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
                        crate::rf_unverified::RfUnverifiedEdge::Entry => {
                            hb_events.emit(
                                crate::rf_unverified::RF_UNVERIFIED_KIND,
                                ados_protocol::logd::Level::Warn,
                                crate::rf_unverified::rf_unverified_detail(
                                    "entry",
                                    &hb_iface,
                                    rates.tx_bytes_per_s,
                                    rates.valid_rx_packets_per_s,
                                    hb_usb_speed,
                                    crate::link_proof::RX_PROOF_GRACE.as_secs(),
                                    None,
                                ),
                            );
                        }
                        crate::rf_unverified::RfUnverifiedEdge::Clear { episode_s } => {
                            hb_events.emit(
                                crate::rf_unverified::RF_UNVERIFIED_KIND,
                                ados_protocol::logd::Level::Info,
                                crate::rf_unverified::rf_unverified_detail(
                                    "clear",
                                    &hb_iface,
                                    rates.tx_bytes_per_s,
                                    rates.valid_rx_packets_per_s,
                                    hb_usb_speed,
                                    crate::link_proof::RX_PROOF_GRACE.as_secs(),
                                    Some(episode_s),
                                ),
                            );
                        }
                        crate::rf_unverified::RfUnverifiedEdge::None => {}
                    }
                    // ── Regulatory status + periodic reconcile ────────────
                    // Refresh the cached live domain at most once per
                    // REG_RECONCILE_INTERVAL. When it drifted off the wanted
                    // value (the injection PHY can re-assert its baked country
                    // as the global domain on a later monitor/bind re-entry),
                    // re-assert it so the onboard WiFi is never left under a
                    // foreign domain. SAFETY: only re-assert when the wanted
                    // domain permits the rendezvous channel — the enabled set
                    // already excludes DFS / disabled channels, so membership
                    // (or an empty/unknown set) is the same gate the bring-up
                    // used; this can never cap the radio.
                    if now.saturating_duration_since(last_reg_read) >= REG_RECONCILE_INTERVAL {
                        last_reg_read = now;
                        reg_status =
                            crate::adapter::read_reg_status(&hb_wanted_domain).await;
                        if !reg_status.verified {
                            let channel_ok = hb_enabled.is_empty()
                                || hb_enabled.contains(&hb_rendezvous);
                            if let crate::adapter::ReassertOutcome::Reasserted {
                                from,
                                to,
                                ..
                            } = crate::adapter::reconcile_reg_domain(
                                &hb_wanted_domain,
                                hb_rendezvous,
                                channel_ok,
                            )
                            .await
                            {
                                hb_events.emit(
                                    crate::reg_reassert::REG_REASSERT_KIND,
                                    crate::reg_reassert::REG_REASSERT_SEVERITY,
                                    crate::reg_reassert::reg_reassert_detail(
                                        &hb_iface,
                                        from.as_deref(),
                                        &to,
                                        hb_rendezvous,
                                        true,
                                    ),
                                );
                                reg_status = crate::adapter::read_reg_status(
                                    &hb_wanted_domain,
                                )
                                .await;
                            }
                        }
                    }

                    let reg = RegSnapshot {
                        domain: reg_status.domain.clone(),
                        verified: reg_status.verified,
                        enabled_channels: hb_enabled.clone(),
                        posture: hb_reg_posture.clone(),
                    };

                    // Build the full body once and write the sidecar (the
                    // live fallback the REST route reads) every tick. The
                    // same body goes to the logging store as one snapshot
                    // event only when a key field changed or 10 s passed.
                    // Best-effort: an absent logging daemon drops the event
                    // without disturbing the radio loop.
                    let tx_dbm = hb_tx_power.effective().unwrap_or(effective_tx_dbm);
                    let body = build_stats_value(
                        state.as_str(),
                        &channels,
                        &reg,
                        tx_dbm,
                        Some(&hb_adapter),
                        &stats,
                        &hb_cfg,
                        hb_restart.load(Ordering::Relaxed),
                        &wd,
                        &rates,
                        &bsnap,
                    );
                    crate::paths::write_sidecar(
                        &crate::paths::run_path("wfb-stats.json"),
                        &body,
                    )
                    .ok();
                    let key = status_emit::StatusKey {
                        state: state.as_str().to_string(),
                        actual_channel: channels.actual,
                        operating_channel: channels.operating,
                        reg_domain: reg.domain.clone(),
                        reg_verified: reg.verified,
                        tx_power_dbm: tx_dbm,
                        tx_power_readback_dbm: readback,
                    };
                    if status_gate.should_emit(key, now) {
                        hb_metrics.emit_event(
                            "link.wfb_status",
                            ados_protocol::logd::Level::Info,
                            json_object_to_fields(&body),
                        );
                    }
                }
                _ = hb_cancel.wait() => break,
            }
        }
    })
}
