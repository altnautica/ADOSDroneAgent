//! FHSS hop supervisor + the always-on control-plane proof listener + presence
//! beacon emitter, plus the Contract E hop/peer sidecar writers and the reactive
//! hop decision.
//!
//! A dedicated 5810 listener decodes the control plane (HopAck + the peer's
//! PresenceBeacon) and drives the shared `HopState`; the hop loop announces a
//! target, waits for the matching ACK, then executes the channel change. The
//! presence-beacon emitter and the proof-only listener cover the
//! `auto_hop_enabled: false` case so the received-side lock proof works
//! regardless of hop config.

use ados_protocol::shutdown::Shutdown;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::hop::{
    build_hop_announce, build_presence_beacon, delay_until_ms, hop_announce_interval,
    hop_announce_rounds, now_unix_ms, parse_hop_ack, parse_presence_beacon, HopState, HopTrigger,
    HOP_ACK_PORT, HOP_CONTROL_PORT, HOP_COUNTDOWN, PRESENCE_INTERVAL,
};
use crate::link_quality::LinkStats;
use crate::paths::{read_bind_sentinel_active, run_path, write_sidecar, WFB_TX_KEY};
use crate::process::RadioProcesses;
use crate::radio_cmd::{HopVerdict, ManualHopRequest};

use crate::config::WfbConfig;

use crate::daemon::bringup::phy::{channel_from_iface, set_channel};

mod execute;
mod presence;
mod sidecars;

use execute::*;
use presence::*;
pub(crate) use presence::{emit_presence_beacons, proof_only_listener};
pub(crate) use sidecars::write_hop_key_unavailable;
use sidecars::*;

/// Peer-beacon freshness window: skip the periodic scan when the peer was heard
/// within this many seconds (the scan locks the radio and drops TX frames). Kept
/// below `PEER_STALE_SECS` (25 s) so a real periodic-scan window exists: a scan
/// only runs when the peer is going quiet (heard between this window and the
/// stale threshold) but the link is still considered up by `can_hop`. With this
/// at or above the stale threshold the periodic path was unreachable — `can_hop`
/// already required a non-stale peer, which is always fresher than the skip
/// window, so every tick hit the skip and the periodic hop never executed.
pub(crate) const PEER_FRESH_SKIP_SECS: f64 = 12.0;

/// Decide whether a reactive hop should fire from the live link stats.
///
/// `cooldown_allowed` is `HopState::reactive_allowed()` (link established + the
/// 30 s reactive cooldown met). A reactive hop fires only on REAL data: the
/// stats RX must have produced a non-empty timestamp AND a non-zero packet
/// count, because the default `LinkStats` (rssi -100, 0 packets, empty
/// timestamp) would otherwise trip the RSSI threshold and hop every cycle on a
/// drone-only rig that never runs the stats RX (no rx.key). With real data, a
/// hop fires when loss or RSSI crosses its configured threshold.
pub(crate) fn reactive_should_fire(
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

/// Decide an operator-initiated manual hop request's verdict. Pure (no I/O), so
/// every rejection path is unit-testable without standing up the supervisor.
///
/// Order matches the contract: not paired → no peer → mid-bind → channel not in
/// the regulatory-enabled set → accepted. `peer_ready` is "a peer has been seen
/// and is not stale" (the GS must be reachable to ACK + follow the announce).
/// `enabled` is the regulatory-permitted set the supervisor holds; `None` (or an
/// empty set) means "could not determine" → do not restrict (the same gate the
/// reactive/periodic hop target filter uses).
pub(crate) fn manual_hop_verdict(
    target: u8,
    paired: bool,
    mid_bind: bool,
    peer_ready: bool,
    enabled: Option<&std::collections::BTreeSet<u8>>,
) -> HopVerdict {
    if !paired {
        return HopVerdict::Rejected {
            reason: "not paired",
        };
    }
    if !peer_ready {
        return HopVerdict::Rejected { reason: "no peer" };
    }
    if mid_bind {
        return HopVerdict::Rejected { reason: "mid-bind" };
    }
    // The regulatory-enabled set forbids channels that would split the pair onto
    // divergent frequencies (`iw set channel` -22). An empty/None set means the
    // wiphy list was unreadable → do not restrict.
    if let Some(set) = enabled {
        if !set.is_empty() && !set.contains(&target) {
            return HopVerdict::Rejected {
                reason: "invalid channel",
            };
        }
    }
    HopVerdict::Accepted { channel: target }
}

/// FHSS hop supervisor. A dedicated 5810 listener decodes the control plane
/// (HopAck + the peer's PresenceBeacon) and drives the shared `HopState`; the
/// hop loop announces a target, waits for the matching ACK, then executes the
/// channel change. Writes `hop-supervisor.json` (5 s) + `peer-presence.json`.
///
/// `state` is process-lifetime (created above the respawn loop), so a
/// whole-group respawn keeps the committed hop and the peer record instead of
/// resetting to the rendezvous home.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_hop_supervisor(
    iface: &str,
    cfg: &WfbConfig,
    proc: Arc<tokio::sync::Mutex<RadioProcesses>>,
    pair_key: &[u8; 32],
    device_id: &str,
    link: Arc<tokio::sync::Mutex<LinkStats>>,
    enabled_channels: Arc<std::collections::BTreeSet<u8>>,
    restart_count: Arc<AtomicU64>,
    rx_proof: crate::link_proof::RxProof,
    proof_reference: Instant,
    operating_channel: Arc<AtomicU64>,
    state: Arc<tokio::sync::Mutex<HopState>>,
    mut manual_rx: tokio::sync::mpsc::Receiver<ManualHopRequest>,
    cancel: Shutdown,
) {
    // The unattended periodic-execution path is opt-in (off by default). The
    // reactive hop + the GS-coordinated follow run regardless; this only gates the
    // time-based periodic scan+hop and drives the honest sidecar `enabled` flag.
    let periodic_hop_enabled = cfg.auto_hop_enabled && cfg.periodic_hop_enabled;
    let pair_key = *pair_key; // [u8;32] is Copy — move into tasks freely.
                              // None when the adapter's reg domain could not be read (do not restrict);
                              // Some(&set) drives the hop-target intersection + the sidecar field.
    let enabled_opt: Option<&std::collections::BTreeSet<u8>> =
        (!enabled_channels.is_empty()).then(|| enabled_channels.as_ref());

    // ── Control-plane listener on 5810: HopAck vs PresenceBeacon ──────────
    let Some(ack_sock) = bind_ack_socket(&cancel).await else {
        return;
    };
    let ack_sock = Arc::new(ack_sock);
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
                        // On a NEW peer device-id, latch it so the persisted pair
                        // state learns the peer over the radio (the bind tunnel
                        // does not always carry it). The latch is a small additive
                        // sidecar the front's peer-backfill reconciler reads and
                        // persists — Rust does not round-trip config.yaml from
                        // here, because the front already owns every write to it.
                        if last_backfilled.as_deref() != Some(peer_id.as_str()) {
                            write_peer_backfill_json(&peer_id);
                            last_backfilled = Some(peer_id);
                        }
                    }
                }
                _ = lst_cancel.wait() => break,
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
                _ = t.tick() => write_hop_supervisor_json(
                    &hb_state,
                    &hb_cfg,
                    &hb_enabled,
                    periodic_hop_enabled,
                ).await,
                _ = hb_cancel.wait() => break,
            }
        }
    });

    let announce_sock = match tokio::net::UdpSocket::bind("0.0.0.0:0").await {
        Ok(s) => s,
        Err(_) => {
            listener.abort();
            hb_writer.abort();
            cancel.wait().await;
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
                // The unattended periodic-execution path is opt-in: it stays off
                // until the GS-coordinated follow is proven on a two-node rig. The
                // reactive hop + the GS follow are unaffected by this gate.
                if !periodic_hop_enabled {
                    continue;
                }
                if !state.lock().await.can_hop() {
                    continue;
                }
                // Skip the periodic scan while the peer is fresh: the scan locks
                // the radio for several seconds and drops wfb_tx frames, so on a
                // healthy link the rescan is pure waste. A reactive scan (the
                // other arm) always runs.
                if state.lock().await.peer_fresh_within(PEER_FRESH_SKIP_SECS) {
                    continue;
                }
                let cur = state.lock().await.channel;
                // Scan live for the quietest enabled in-band channel (rotates if
                // the scan is flat, e.g. monitor mode rejected it).
                let target =
                    crate::channel::pick_hop_target(iface, cur, &cfg.band, enabled_opt).await;
                // The scan can strand the iface in managed mode on some drivers;
                // re-assert monitor mode + retune regardless of whether a hop
                // follows so wfb_tx keeps injecting.
                crate::adapter::restore_monitor_if_needed(iface, cur).await;
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
                    let target = crate::channel::pick_hop_target(
                        iface, cur, &cfg.band, enabled_opt,
                    )
                    .await;
                    crate::adapter::restore_monitor_if_needed(iface, cur).await;
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
                    // Always attempt the channel set AND the respawn (never leave
                    // the radio group dead), but a silent `iw set channel` failure
                    // makes the recorded outcome false: respawning the radio on
                    // the wrong channel is not a successful return home. The
                    // group respawn reuses the live data-plane tunables (manual
                    // tier / adaptive FEC), so a return home never reverts the
                    // operator's pinned link rate back to the boot-time config.
                    let channel_ok = set_channel(iface, home).await;
                    let spawn_ok = {
                        let mut p = proc.lock().await;
                        let ok = p.respawn_group(cfg, link.clone()).await;
                        if ok {
                            restart_count.fetch_add(1, Ordering::Relaxed);
                        } else {
                            tracing::warn!("return_home_restart_failed");
                        }
                        ok
                    };
                    state.lock().await.record_return_home(channel_ok && spawn_ok);
                }

                // Keep the shared operating channel in sync with the hop state's
                // current channel, so the heartbeat's `operating_channel` field
                // reflects a committed move. It equals the rendezvous home until a
                // hop changes it (and returns to home on the return-home path).
                operating_channel.store(state.lock().await.channel as u64, Ordering::Relaxed);
            }
            // Operator-initiated coordinated hop from the radio command socket.
            // Validate (paired / peer present / not mid-bind / channel in the
            // enabled set), REPLY to the requester immediately, then on
            // acceptance drive the hop through the EXISTING announce path so the
            // GS follows. The reply is sent before the multi-second announce so
            // the socket connection never blocks on the air handshake.
            maybe_req = manual_rx.recv() => {
                let Some(req) = maybe_req else {
                    // The sender dropped (the command socket task ended). Stop
                    // polling this arm by swapping in a never-ready receiver so
                    // the select keeps serving the other arms.
                    let (_tx, never) = tokio::sync::mpsc::channel::<ManualHopRequest>(1);
                    manual_rx = never;
                    continue;
                };
                let target = req.channel;
                let verdict = {
                    let s = state.lock().await;
                    manual_hop_verdict(
                        target,
                        Path::new(WFB_TX_KEY).exists(),
                        read_bind_sentinel_active(),
                        s.peer().is_some() && !s.peer_is_stale(),
                        enabled_opt,
                    )
                };
                // Reply BEFORE the announce so the socket returns promptly. A
                // dropped receiver (the client hung up) is fine — the hop still
                // runs on acceptance.
                let accepted = matches!(verdict, HopVerdict::Accepted { .. });
                let _ = req.reply.send(verdict);
                if accepted {
                    tracing::info!(target, "hop_manual_trigger");
                    // Re-assert monitor + retune is folded into try_execute_hop's
                    // set_channel; the manual path uses the SAME announce + ACK +
                    // dwell-sync as the periodic/reactive paths (never bypassed).
                    try_execute_hop(
                        iface, cfg, &proc, &state, &announce_sock, &mut ack_rx, &pair_key,
                        target, HopTrigger::Manual, "manual", &link, &restart_count,
                    )
                    .await;
                    operating_channel.store(state.lock().await.channel as u64, Ordering::Relaxed);
                }
            }
            _ = cancel.wait() => {
                listener.abort();
                hb_writer.abort();
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests;
