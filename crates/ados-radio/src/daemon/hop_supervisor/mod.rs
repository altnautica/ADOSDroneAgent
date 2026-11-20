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
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::hop::{
    build_hop_announce, build_presence_beacon, delay_until_ms, hop_announce_interval,
    hop_announce_rounds, now_unix_ms, parse_hop_ack, parse_presence_beacon, HopState, HopTrigger,
    HOP_ACK_PORT, HOP_COUNTDOWN, PRESENCE_INTERVAL,
};
use crate::link_proof::RxProof;
use crate::link_quality::LinkStats;
use crate::process::RadioProcesses;
use crate::radio_cmd::{HopVerdict, ManualHopRequest};

use crate::config::WfbConfig;

mod execute;
mod presence;
mod radio;
mod sidecars;

use execute::*;
pub use presence::emit_presence_beacons_on;
use presence::*;
pub(crate) use presence::{emit_presence_beacons, proof_only_listener};
pub use radio::{ControlPlane, HopRadio};
use radio::{LiveRadio, LoopbackControl};
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

/// FHSS hop supervisor on the drone's own radio: binds the announce socket and
/// runs [`run_hop_loop`] over [`LiveRadio`]. Waits for `cancel` when the
/// announce socket cannot be bound.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_hop_supervisor(
    iface: &str,
    cfg: &WfbConfig,
    proc: Arc<tokio::sync::Mutex<RadioProcesses>>,
    pair_key: &[u8; 32],
    device_id: &str,
    link: Arc<tokio::sync::Mutex<LinkStats>>,
    enabled_channels: Arc<BTreeSet<u8>>,
    restart_count: Arc<AtomicU64>,
    rx_proof: RxProof,
    proof_reference: Instant,
    operating_channel: Arc<AtomicU64>,
    state: Arc<tokio::sync::Mutex<HopState>>,
    manual_rx: tokio::sync::mpsc::Receiver<ManualHopRequest>,
    cancel: Shutdown,
) {
    let Ok(control) = LoopbackControl::bind(iface).await else {
        cancel.wait().await;
        return;
    };
    let radio = Arc::new(LiveRadio {
        control,
        cfg: cfg.clone(),
        proc,
        link: link.clone(),
    });
    let ctx = HopContext {
        cfg: cfg.clone(),
        pair_key: *pair_key,
        device_id: device_id.to_string(),
        link,
        enabled_channels,
        restart_count,
        rx_proof,
        proof_reference,
        operating_channel,
        state,
    };
    run_hop_loop(radio, ctx, manual_rx, cancel).await;
}

/// What one hop-supervisor generation shares with the rest of the radio
/// service.
pub struct HopContext {
    pub cfg: WfbConfig,
    /// The hop/presence pair key.
    pub pair_key: [u8; 32],
    /// This rig's device-id, for the self-beacon drop.
    pub device_id: String,
    /// The live uplink stats the reactive trigger reads.
    pub link: Arc<tokio::sync::Mutex<LinkStats>>,
    /// The regulatory-permitted channel set; empty means unknown (do not
    /// restrict).
    pub enabled_channels: Arc<BTreeSet<u8>>,
    /// Radio-group respawns since service start.
    pub restart_count: Arc<AtomicU64>,
    /// The received-side proof a verified return frame records.
    pub rx_proof: RxProof,
    pub proof_reference: Instant,
    /// The channel the heartbeat reports as operating.
    pub operating_channel: Arc<AtomicU64>,
    /// Process-lifetime hop state (created above the respawn loop), so a
    /// whole-group respawn keeps the committed hop and the peer record instead
    /// of resetting to the rendezvous home.
    pub state: Arc<tokio::sync::Mutex<HopState>>,
}

/// FHSS hop supervisor. A dedicated 5810 listener decodes the control plane
/// (HopAck + the peer's PresenceBeacon) and drives the shared `HopState`; the
/// hop loop announces a target, waits for the matching ACK, then executes the
/// channel change. Writes `hop-supervisor.json` (5 s) + `peer-presence.json`.
pub async fn run_hop_loop<R: HopRadio>(
    radio: Arc<R>,
    ctx: HopContext,
    mut manual_rx: tokio::sync::mpsc::Receiver<ManualHopRequest>,
    cancel: Shutdown,
) {
    let HopContext {
        cfg,
        pair_key,
        device_id,
        link,
        enabled_channels,
        restart_count,
        rx_proof,
        proof_reference,
        operating_channel,
        state,
    } = ctx;
    let cfg = &cfg;
    // The unattended periodic-execution path is opt-in (off by default). The
    // reactive hop + the GS-coordinated follow run regardless; this only gates the
    // time-based periodic scan+hop and drives the honest sidecar `enabled` flag.
    let periodic_hop_enabled = cfg.auto_hop_enabled && cfg.periodic_hop_enabled;
    // None when the adapter's reg domain could not be read (do not restrict);
    // Some(&set) drives the hop-target intersection + the sidecar field.
    let enabled_opt: Option<&BTreeSet<u8>> =
        (!enabled_channels.is_empty()).then(|| enabled_channels.as_ref());

    // ── Control-plane listener on 5810: HopAck vs PresenceBeacon ──────────
    let Some(ack_port) = bind_return_port(&*radio, &cancel).await else {
        return;
    };
    // The listener and the sidecar writer live exactly as long as this
    // supervisor: dropping the set aborts them. The service aborts the
    // supervisor on every radio-group respawn without firing `cancel`, and a
    // listener that outlived it kept the return port bound, so every later
    // generation retried the bind forever and never hopped, returned home or
    // answered a manual hop again.
    let mut children = tokio::task::JoinSet::new();
    // Acked target channels flow from the listener to the hop loop.
    let (ack_tx, mut ack_rx) = tokio::sync::mpsc::channel::<u8>(8);
    children.spawn(listen_return(
        radio.clone(),
        ack_port,
        pair_key,
        device_id,
        state.clone(),
        rx_proof,
        proof_reference,
        ack_tx,
        cancel.clone(),
    ));

    // ── hop-supervisor.json writer (5 s) ──────────────────────────────────
    let hb_radio = radio.clone();
    let hb_state = state.clone();
    let hb_cancel = cancel.clone();
    let hb_cfg = cfg.clone();
    let hb_enabled = enabled_channels.clone();
    children.spawn(async move {
        let mut t = tokio::time::interval(Duration::from_secs(5));
        loop {
            tokio::select! {
                _ = t.tick() => write_hop_supervisor_json(
                    &*hb_radio,
                    &hb_state,
                    &hb_cfg,
                    &hb_enabled,
                    periodic_hop_enabled,
                ).await,
                _ = hb_cancel.wait() => break,
            }
        }
    });

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
        // `biased`: the arms are polled in this fixed order, so one sequence of
        // inputs gives one schedule (the deterministic simulation replays a seed
        // byte for byte). Every arm is a periodic tick or a rare request, so
        // the fixed order starves none.
        tokio::select! {
            biased;
            _ = hop_tick.tick() => {
                // Never change channel while a bind owns the adapter: the bind
                // stops the normal wfb unit so its bind profile can own the radio
                // exclusively, and a racing iw-channel + wfb_tx restart would
                // corrupt the bind key exchange. The sentinel is a cheap sync
                // file read, so no socket round-trip on this hot tick.
                if radio.bind_active() {
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
                let target = radio.pick_hop_target(cur, enabled_opt).await;
                // The scan can strand the iface in managed mode on some drivers;
                // re-assert monitor mode + retune regardless of whether a hop
                // follows so wfb_tx keeps injecting.
                radio.restore_monitor_if_needed(cur).await;
                if target == cur {
                    continue;
                }
                try_execute_hop(
                    &*radio, &state, &mut ack_rx, &pair_key,
                    target, HopTrigger::Periodic, "periodic", &restart_count,
                )
                .await;
            }
            // Reactive trigger + peer-stale return-to-home (every 5 s).
            _ = stale_tick.tick() => {
                // Suppress all actuation during a bind, same as the periodic arm.
                if radio.bind_active() {
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
                    let target = radio.pick_hop_target(cur, enabled_opt).await;
                    radio.restore_monitor_if_needed(cur).await;
                    if target != cur {
                        tracing::info!(target, "hop_reactive_trigger");
                        try_execute_hop(
                            &*radio, &state, &mut ack_rx, &pair_key,
                            target, HopTrigger::Reactive, "reactive", &restart_count,
                        )
                        .await;
                    }
                }

                let (return_home, home) = {
                    let s = state.lock().await;
                    (s.should_return_home(), s.home_channel)
                };
                // Re-read the sentinel: the reactive hop above can take seconds,
                // and a bind that opened meanwhile owns the adapter.
                if return_home && !radio.bind_active() {
                    tracing::info!(home, "hop_return_home");
                    // Always attempt the channel set AND the respawn (never leave
                    // the radio group dead), but a silent `iw set channel` failure
                    // makes the recorded outcome false: respawning the radio on
                    // the wrong channel is not a successful return home.
                    let channel_ok = radio.set_channel(home).await;
                    let spawn_ok = radio.respawn_group().await;
                    if spawn_ok {
                        restart_count.fetch_add(1, Ordering::Relaxed);
                    } else {
                        tracing::warn!("return_home_restart_failed");
                    }
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
                        radio.paired(),
                        radio.bind_active(),
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
                        &*radio, &state, &mut ack_rx, &pair_key,
                        target, HopTrigger::Manual, "manual", &restart_count,
                    )
                    .await;
                    operating_channel.store(state.lock().await.channel as u64, Ordering::Relaxed);
                }
            }
            _ = cancel.wait() => {
                children.abort_all();
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests;
