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

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::sync::Notify;

use ados_radio::hop::{
    build_hop_announce, build_presence_beacon, hop_announce_interval, hop_announce_rounds,
    hop_epoch_ms, parse_hop_ack, parse_presence_beacon, HopState, HopTrigger, HOP_ACK_PORT,
    HOP_CONTROL_PORT, PRESENCE_INTERVAL,
};
use ados_radio::link_quality::LinkStats;
use ados_radio::paths::{read_bind_sentinel_active, run_path, write_sidecar, WFB_TX_KEY};
use ados_radio::process::RadioProcesses;
use ados_radio::radio_cmd::{HopVerdict, ManualHopRequest};

use ados_radio::config::WfbConfig;

use crate::bringup::{channel_from_iface, set_channel};

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

/// Emit a periodic PresenceBeacon on the control plane so the peer hears this
/// rig even when the hop supervisor is disabled. The live channel is read each
/// tick (falling back to the configured channel) so the beacon advertises where
/// the radio actually is.
pub(crate) async fn emit_presence_beacons(
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
pub(crate) async fn proof_only_listener(
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
pub(crate) async fn run_hop_supervisor(
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
    mut manual_rx: tokio::sync::mpsc::Receiver<ManualHopRequest>,
    cancel: Arc<Notify>,
) {
    let state = Arc::new(tokio::sync::Mutex::new(HopState::new(cfg.channel)));
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
                _ = t.tick() => write_hop_supervisor_json(
                    &hb_state,
                    &hb_cfg,
                    &hb_enabled,
                    periodic_hop_enabled,
                ).await,
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
                    state.lock().await.record_hop(home, "return_home", channel_ok && spawn_ok);
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
    // A silent `iw set channel` failure makes the hop outcome false even when
    // the radio respawns cleanly: a hop that landed on the old channel is not a
    // successful hop. The radio is always respawned so the link is never left
    // dead. The group respawn reuses the live data-plane tunables (manual tier /
    // adaptive FEC) so a hop never silently reverts the operator's link rate.
    let channel_ok = set_channel(iface, target).await;
    let spawn_ok = {
        let mut p = proc.lock().await;
        let ok = p.respawn_group(cfg, link.clone()).await;
        if ok {
            restart_count.fetch_add(1, Ordering::Relaxed);
        }
        ok
    };
    if spawn_ok {
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
    } else {
        state.lock().await.record_hop(target, label, false);
        tracing::warn!(iface, channel = target, "hop_wfb_restart_failed");
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
pub(crate) fn is_self_beacon(own_device_id: &str, beacon_device_id: &str) -> bool {
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
/// The honest reason the time-based periodic hop is not executing, or `None`
/// when it is active. Reported in the hop sidecar so the panel never shows
/// `enabled:true` for a path that cannot fire (the prior sidecar always reported
/// `auto_hop_enabled`, which masked the suppressed periodic path).
pub(crate) fn periodic_hop_suppression_reason(cfg: &WfbConfig) -> Option<&'static str> {
    if !cfg.auto_hop_enabled {
        Some("auto_hop_disabled")
    } else if !cfg.periodic_hop_enabled {
        Some("periodic_hop_opt_in_off")
    } else {
        None
    }
}

async fn write_hop_supervisor_json(
    state: &Arc<tokio::sync::Mutex<HopState>>,
    cfg: &WfbConfig,
    enabled_channels: &std::collections::BTreeSet<u8>,
    periodic_hop_enabled: bool,
) {
    let suppression = periodic_hop_suppression_reason(cfg);
    let v = {
        let s = state.lock().await;
        let history =
            serde_json::to_value(s.history()).unwrap_or_else(|_| serde_json::Value::Array(vec![]));
        let enabled: Vec<u8> = enabled_channels.iter().copied().collect();
        json!({
            // `enabled` is the honest periodic-execution state, not the bare
            // `auto_hop_enabled`: a suppressed periodic path reports false plus a
            // reason. The reactive hop + the GS-coordinated follow run regardless.
            "enabled": periodic_hop_enabled,
            "auto_hop_enabled": cfg.auto_hop_enabled,
            "periodic_hop_enabled": cfg.periodic_hop_enabled,
            "periodic_suppression_reason": suppression,
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

#[cfg(test)]
mod tests {
    use super::*;
    use ados_radio::hop::{build_hop_announce, derive_pair_key, parse_hop_announce, HopState};

    /// The enabled-channel set helper for the verdict tests: the U-NII-3 home
    /// band so a request for 153 is in-set and a request for 36 is out-of-set.
    fn unii3_set() -> std::collections::BTreeSet<u8> {
        [149u8, 153, 157, 161, 165].into_iter().collect()
    }

    #[test]
    fn manual_hop_rejected_when_not_paired() {
        // Not paired wins first, regardless of peer/bind/channel.
        let v = manual_hop_verdict(153, false, false, true, Some(&unii3_set()));
        assert_eq!(
            v,
            HopVerdict::Rejected {
                reason: "not paired"
            }
        );
    }

    #[test]
    fn manual_hop_rejected_when_no_peer_or_stale_peer() {
        // Paired but no live peer → no peer (the GS can't ACK the announce).
        let v = manual_hop_verdict(153, true, false, false, Some(&unii3_set()));
        assert_eq!(v, HopVerdict::Rejected { reason: "no peer" });
    }

    #[test]
    fn manual_hop_rejected_mid_bind() {
        // Paired + peer ready but a bind owns the adapter → mid-bind.
        let v = manual_hop_verdict(153, true, true, true, Some(&unii3_set()));
        assert_eq!(v, HopVerdict::Rejected { reason: "mid-bind" });
    }

    #[test]
    fn manual_hop_rejected_channel_not_in_enabled_set() {
        // 36 is a valid WFB channel (the socket accepts the format) but it is not
        // in the U-NII-3 enabled set this rig holds, so the supervisor refuses it
        // to avoid splitting the pair onto a forbidden frequency.
        let v = manual_hop_verdict(36, true, false, true, Some(&unii3_set()));
        assert_eq!(
            v,
            HopVerdict::Rejected {
                reason: "invalid channel"
            }
        );
    }

    #[test]
    fn manual_hop_accepted_when_paired_peer_ready_in_band() {
        let v = manual_hop_verdict(157, true, false, true, Some(&unii3_set()));
        assert_eq!(v, HopVerdict::Accepted { channel: 157 });
    }

    #[test]
    fn manual_hop_unknown_enabled_set_does_not_restrict() {
        // A None or empty enabled set means the wiphy list was unreadable → do
        // not restrict (the same gate the periodic/reactive target filter uses).
        let v = manual_hop_verdict(36, true, false, true, None);
        assert_eq!(v, HopVerdict::Accepted { channel: 36 });
        let empty = std::collections::BTreeSet::new();
        let v = manual_hop_verdict(36, true, false, true, Some(&empty));
        assert_eq!(v, HopVerdict::Accepted { channel: 36 });
    }

    /// The manual-hop path drives `try_execute_hop` through the SAME announce
    /// path the periodic/reactive triggers use, so the GS follows. Rather than
    /// stand up the radio (set_channel/respawn fork real binaries), this asserts
    /// the exact announce `try_execute_hop` sends for a manual hop — built with
    /// `HopTrigger::Manual` — decodes on the GS-side `parse_hop_announce` as the
    /// "manual" trigger on the announce wire. The announce is what the GS hears
    /// and ACKs; a coordinated follow is impossible if the trigger byte is wrong.
    #[tokio::test]
    async fn manual_trigger_announce_is_decodable_by_the_gs_follow_path() {
        let key = derive_pair_key(None);
        // The drone-side announce socket and a loopback listener standing in for
        // the GS follow path's control-plane reader (the real GS binds 5803 via
        // wfb_rx; here a plain UDP socket proves the wire).
        let announce = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let gs = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let gs_addr = gs.local_addr().unwrap();

        // Exactly what try_execute_hop builds + sends for a manual hop.
        let epoch = ados_radio::hop::hop_epoch_ms();
        let pkt = build_hop_announce(epoch, 153, HopTrigger::Manual, &key);
        announce.send_to(&pkt, gs_addr).await.unwrap();

        let mut buf = [0u8; 128];
        let (n, _) = gs.recv_from(&mut buf).await.unwrap();
        // The GS follow path decodes the announce; a manual trigger must surface
        // as "manual" (so the follow is recorded honestly) on the requested
        // channel.
        assert_eq!(parse_hop_announce(&buf[..n], &key), Some((153, "manual")));
    }

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
    fn periodic_hop_is_opt_in_and_defaults_off() {
        // A default rig has auto_hop_enabled but periodic_hop_enabled off: the
        // unattended periodic-execution path is suppressed with the opt-in reason,
        // and the effective gate the supervisor reads is false.
        let cfg = WfbConfig::default();
        assert!(cfg.auto_hop_enabled);
        assert!(!cfg.periodic_hop_enabled);
        assert_eq!(
            periodic_hop_suppression_reason(&cfg),
            Some("periodic_hop_opt_in_off")
        );
        // The effective gate (auto_hop_enabled && periodic_hop_enabled) is false.
        assert!(!(cfg.auto_hop_enabled && cfg.periodic_hop_enabled));
    }

    #[test]
    fn periodic_hop_suppression_names_the_real_reason() {
        // auto_hop off → the auto-hop reason wins (the whole supervisor is off).
        let auto_off = WfbConfig {
            auto_hop_enabled: false,
            periodic_hop_enabled: true,
            ..WfbConfig::default()
        };
        assert_eq!(
            periodic_hop_suppression_reason(&auto_off),
            Some("auto_hop_disabled")
        );
        // Both on → no suppression (the periodic path is active).
        let both_on = WfbConfig {
            auto_hop_enabled: true,
            periodic_hop_enabled: true,
            ..WfbConfig::default()
        };
        assert_eq!(periodic_hop_suppression_reason(&both_on), None);
    }

    #[test]
    fn periodic_skip_window_is_below_stale_so_the_path_is_reachable() {
        // The periodic hop requires `can_hop()` (peer fresh < PEER_STALE_SECS) AND
        // NOT `peer_fresh_within(PEER_FRESH_SKIP_SECS)`. With the skip window below
        // the stale window there is a real window where both hold: a peer last
        // seen between the skip window and the stale threshold passes can_hop yet
        // is no longer "fresh", so the periodic scan runs (when opted in). If the
        // skip window were >= the stale threshold the path would be unreachable,
        // because a non-stale peer is always inside the (larger) skip window.
        const {
            assert!(
                PEER_FRESH_SKIP_SECS < ados_radio::hop::PEER_STALE_SECS,
                "skip window must be below the stale threshold for the periodic path to be reachable"
            );
        }
        // The reachable in-window peer state (skip < age < stale) is exercised
        // over the public HopState surface in `hop::tests`, which can backdate the
        // peer-seen clock; here the constant ordering is the load-bearing invariant.
    }
}
