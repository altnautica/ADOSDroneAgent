//! Announce, ACK wait and channel-change execution for one hop, plus the
//! call-back announce after a failed retune.

use super::*;

/// Announce a hop to `target`, wait for the matching ACK, and on success
/// execute the channel change (kill the radio group → `iw set channel` →
/// respawn). Records the outcome in the hop history with `label`. Shared by the
/// periodic, reactive and manual triggers.
///
/// The ground station acks only when it can follow, so an ack commits both
/// sides. If this side then fails to retune, the ground station has moved
/// without it: [`reannounce_back`] tells it to come back to the channel this
/// radio is still on.
#[allow(clippy::too_many_arguments)]
pub(super) async fn try_execute_hop(
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
    let from = state.lock().await.channel;
    // The flip is scheduled on this side's monotonic clock; each announce
    // carries the time remaining, so the peer schedules against its own.
    let deadline = tokio::time::Instant::now() + HOP_COUNTDOWN;
    if !announce_until_acked(
        announce_sock,
        ack_rx,
        pair_key,
        target,
        trigger,
        deadline,
        hop_announce_rounds(),
    )
    .await
    {
        return;
    }
    tokio::time::sleep_until(deadline).await;
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
    if !channel_ok {
        reannounce_back(announce_sock, ack_rx, pair_key, from, trigger).await;
    }
}

/// Rounds of the announce that calls the ground station back after a failed
/// retune: long enough to outlast its own no-traffic revert window, so the
/// call is heard whichever of the two brings it back first.
pub(super) const REANNOUNCE_ROUNDS: u32 = 80;

/// Call the ground station back to `channel`, the one this radio is still on.
///
/// The ground station acked and retuned; this side's retune failed. It reverts
/// on its own when no traffic arrives on the new channel, but that takes
/// seconds and a ground station that cannot verify traffic would stay. The
/// announce is the explicit instruction, and costs nothing if it went back
/// already.
pub(super) async fn reannounce_back(
    announce_sock: &tokio::net::UdpSocket,
    ack_rx: &mut tokio::sync::mpsc::Receiver<u8>,
    pair_key: &[u8; 32],
    channel: u8,
    trigger: HopTrigger,
) {
    tracing::warn!(channel, "hop_retune_failed_calling_peer_back");
    let deadline = tokio::time::Instant::now() + HOP_COUNTDOWN;
    let acked = announce_until_acked(
        announce_sock,
        ack_rx,
        pair_key,
        channel,
        trigger,
        deadline,
        REANNOUNCE_ROUNDS,
    )
    .await;
    if acked {
        tracing::info!(channel, "hop_peer_called_back");
    }
}

/// Send the announce for `target` up to `rounds` times, 100 ms apart, each
/// carrying the milliseconds left until `deadline`; stop on the matching ACK.
/// Returns whether the peer acked.
pub(super) async fn announce_until_acked(
    announce_sock: &tokio::net::UdpSocket,
    ack_rx: &mut tokio::sync::mpsc::Receiver<u8>,
    pair_key: &[u8; 32],
    target: u8,
    trigger: HopTrigger,
    deadline: tokio::time::Instant,
    rounds: u32,
) -> bool {
    // Drain stale acks so we only count one for THIS announce.
    while ack_rx.try_recv().is_ok() {}
    for _ in 0..rounds {
        let delay = delay_until_ms(deadline.into_std(), std::time::Instant::now());
        let pkt = build_hop_announce(delay, target, trigger, pair_key);
        let _ = announce_sock
            .send_to(&pkt, format!("127.0.0.1:{HOP_CONTROL_PORT}"))
            .await;
        if let Ok(Some(ch)) = tokio::time::timeout(hop_announce_interval(), ack_rx.recv()).await {
            if ch == target {
                return true;
            }
        }
    }
    false
}
