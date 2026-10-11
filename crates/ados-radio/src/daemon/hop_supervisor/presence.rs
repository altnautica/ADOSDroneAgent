//! Presence beacon emitter, the control-plane return-port bind and the
//! proof-only listener used when the hop supervisor is disabled.

use super::*;

/// Emit a periodic PresenceBeacon on the control plane so the peer hears this
/// rig even when the hop supervisor is disabled. The live channel is read each
/// tick (falling back to the configured channel) so the beacon advertises where
/// the radio actually is.
pub(crate) async fn emit_presence_beacons(
    device_id: &str,
    iface: &str,
    fallback_channel: u8,
    pair_key: &[u8; 32],
    cancel: Shutdown,
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
                let epoch = now_unix_ms();
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
            _ = cancel.wait() => return,
        }
    }
}

/// Retry cadence for binding the control-plane return port. Fixed and uncapped:
/// a port briefly held by a restarting sibling must not cost the received-side
/// proof for the rest of the bring-up.
pub(super) const ACK_BIND_RETRY: Duration = Duration::from_secs(5);

/// Bind the control-plane return port (`127.0.0.1:5810`), retrying every
/// [`ACK_BIND_RETRY`] until it binds or `cancel` fires (`None`). Loopback only:
/// the one producer is the rx-control `wfb_rx` re-emitting on 127.0.0.1, and
/// nothing on the network should be able to post into this listener.
pub(super) async fn bind_ack_socket(cancel: &Shutdown) -> Option<tokio::net::UdpSocket> {
    loop {
        match tokio::net::UdpSocket::bind(("127.0.0.1", HOP_ACK_PORT)).await {
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
    let Some(sock) = bind_ack_socket(&cancel).await else {
        return;
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
