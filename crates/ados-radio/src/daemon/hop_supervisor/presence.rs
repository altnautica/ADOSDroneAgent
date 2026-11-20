//! Presence beacon emitter, the control-plane return-port bind and listener,
//! and the proof-only listener used when the hop supervisor is disabled.

use super::*;

/// Emit a periodic PresenceBeacon on the control plane so the peer hears this
/// rig even when the hop supervisor is disabled. Ends at once when the send
/// socket cannot be bound.
pub(crate) async fn emit_presence_beacons(
    device_id: &str,
    iface: &str,
    fallback_channel: u8,
    pair_key: &[u8; 32],
    cancel: Shutdown,
) {
    let Ok(control) = LoopbackControl::bind(iface).await else {
        return;
    };
    emit_presence_beacons_on(&control, device_id, fallback_channel, pair_key, cancel).await;
}

/// The presence-beacon loop over `control`. The live channel is read each tick
/// (falling back to the last one read, first the configured channel) so the
/// beacon advertises where the radio actually is.
pub async fn emit_presence_beacons_on<C: ControlPlane>(
    control: &C,
    device_id: &str,
    fallback_channel: u8,
    pair_key: &[u8; 32],
    cancel: Shutdown,
) {
    let mut tick = tokio::time::interval(PRESENCE_INTERVAL);
    let mut last_live = fallback_channel;
    loop {
        tokio::select! {
            _ = tick.tick() => {
                if let Some(live) = control.live_channel().await {
                    last_live = live;
                }
                let epoch = now_unix_ms();
                let pkt = build_presence_beacon(
                    device_id,
                    true, // drone role
                    last_live,
                    0,    // rssi not known at drone-side TX
                    epoch,
                    pair_key,
                );
                control.send_control(&pkt).await;
            }
            _ = cancel.wait() => return,
        }
    }
}

/// Retry cadence for binding the control-plane return port. Fixed and uncapped:
/// a port briefly held by a restarting sibling must not cost the received-side
/// proof for the rest of the bring-up.
pub(super) const ACK_BIND_RETRY: Duration = Duration::from_secs(5);

/// Bind the control-plane return port (`127.0.0.1:5810`), retrying every
/// [`ACK_BIND_RETRY`] until it binds or `cancel` fires (`None`).
pub(super) async fn bind_return_port<C: ControlPlane>(
    control: &C,
    cancel: &Shutdown,
) -> Option<C::Port> {
    loop {
        match control.bind_return_port().await {
            Ok(s) => return Some(s),
            Err(e) => {
                tracing::warn!(error = %e, port = HOP_ACK_PORT, "hop_ack_socket_bind_failed_retrying");
                tokio::select! {
                    _ = tokio::time::sleep(ACK_BIND_RETRY) => {}
                    _ = cancel.wait() => return None,
                }
            }
        }
    }
}

/// The control-plane listener: a verified HopAck goes to the hop loop, a
/// verified peer PresenceBeacon updates the shared hop state. Both are
/// received-side proof.
#[allow(clippy::too_many_arguments)]
pub(super) async fn listen_return<R: HopRadio>(
    radio: Arc<R>,
    port: R::Port,
    pair_key: [u8; 32],
    // Own device-id, for the self-beacon drop. The beacon carries a 16-byte
    // device-id, so the loopback-delivered copy of this rig's own beacon is
    // recognised by its first 16 bytes.
    own_device_id: String,
    state: Arc<tokio::sync::Mutex<HopState>>,
    // The shared received-side proof: a verified HopAck or a peer PresenceBeacon
    // is the drone's evidence that its energy reached a receiver. The heartbeat
    // reads this to compute `channel_locked` / `rf_unverified`.
    rx_proof: RxProof,
    proof_reference: Instant,
    ack_tx: tokio::sync::mpsc::Sender<u8>,
    cancel: Shutdown,
) {
    let mut buf = [0u8; 128];
    // Tracks the last peer device-id back-filled so the cross-process write
    // only fires on a CHANGE (matches the Python `previous != device_id`).
    let mut last_backfilled: Option<String> = None;
    loop {
        tokio::select! {
            r = radio.recv_return(&port, &mut buf) => {
                let Ok(n) = r else { continue };
                let pkt = &buf[..n];
                if let Some(target) = parse_hop_ack(pkt, &pair_key) {
                    // A verified ack from the peer is received-side proof, and
                    // proof the peer is present on this channel.
                    rx_proof.observe(Instant::now(), proof_reference);
                    let _ = ack_tx.try_send(target);
                } else if let Some(p) = parse_presence_beacon(pkt, &pair_key) {
                    // Drop this rig's own beacon (a loopback race can deliver
                    // it) so a self-beacon never registers as a peer.
                    if is_self_beacon(&own_device_id, &p.device_id) {
                        continue;
                    }
                    // A verified peer beacon is received-side proof too.
                    rx_proof.observe(Instant::now(), proof_reference);
                    let peer_id = p.device_id.clone();
                    state.lock().await.on_peer_beacon(p);
                    write_peer_presence_json(&*radio, &state).await;
                    // On a NEW peer device-id, latch it so the persisted pair
                    // state learns the peer over the radio (the bind tunnel
                    // does not always carry it). The latch is a small additive
                    // sidecar the front's peer-backfill reconciler reads and
                    // persists — Rust does not round-trip config.yaml from
                    // here, because the front already owns every write to it.
                    if last_backfilled.as_deref() != Some(peer_id.as_str()) {
                        write_peer_backfill_json(&*radio, &peer_id);
                        last_backfilled = Some(peer_id);
                    }
                }
            }
            _ = cancel.wait() => break,
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
    rx_proof: crate::link_proof::RxProof,
    reference: Instant,
    cancel: Shutdown,
) {
    let control = LoopbackControl::receive_only();
    let Some(sock) = bind_return_port(&control, &cancel).await else {
        return;
    };
    let pair_key = *pair_key;
    let own_device_id = device_id.to_string();
    let mut buf = [0u8; 128];
    loop {
        tokio::select! {
            r = control.recv_return(&sock, &mut buf) => {
                let Ok(n) = r else { continue };
                let pkt = &buf[..n];
                if parse_hop_ack(pkt, &pair_key).is_some() {
                    rx_proof.observe(Instant::now(), reference);
                } else if let Some(p) = parse_presence_beacon(pkt, &pair_key) {
                    if !is_self_beacon(&own_device_id, &p.device_id) {
                        rx_proof.observe(Instant::now(), reference);
                    }
                }
            }
            _ = cancel.wait() => return,
        }
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
