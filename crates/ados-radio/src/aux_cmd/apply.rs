//! Applying validated commands to the aux lane.

use super::*;

/// Apply a validated command to the lane this state carries.
pub(super) async fn apply(cmd: Command, state: &AuxCmdState) -> Value {
    match cmd {
        Command::Open => {
            // The operator dead-switch is checked first so a disabled deployment
            // gets a clear, distinct error rather than a generic open failure,
            // and so NO process is spawned (the lock + open below is never
            // reached). (`open_aux_stream` enforces the same guard structurally,
            // so a cap-holding caller can never open the stream when disabled.)
            if let Some(reply) = aux_disabled_reply(state.aux_enable) {
                return reply;
            }
            match &state.lane {
                AuxLane::Radio { proc, cfg } => {
                    // Idempotent open: brings up the additive aux pair on the
                    // config's effective aux ports/FEC/MCS. Never touches the
                    // data/control planes.
                    if proc.lock().await.open_aux_stream(cfg).await {
                        json!({
                            "ok": true,
                            "active": true,
                            "tx_port": cfg.aux_tx_port,
                            "rx_port": cfg.aux_rx_port,
                        })
                    } else {
                        json!({"ok": false, "error": "E_AUX_OPEN_FAILED"})
                    }
                }
                // The uplink transmitter already runs; opening admits this
                // socket's sends. No `rx_port`: inbound traffic reaches a client
                // only through `subscribe`, fed per drone slot in-process, so no
                // single receive port describes it.
                AuxLane::Uplink { tx_port, open, .. } => {
                    open.store(true, Ordering::Release);
                    json!({"ok": true, "active": true, "tx_port": tx_port})
                }
            }
        }
        Command::Close => {
            match &state.lane {
                AuxLane::Radio { proc, .. } => proc.lock().await.close_aux_stream().await,
                // Closes the gate, never the shared uplink transmitter.
                AuxLane::Uplink { open, .. } => open.store(false, Ordering::Release),
            }
            json!({"ok": true, "active": false})
        }
        Command::Status => {
            let active = match &state.lane {
                AuxLane::Radio { proc, .. } => proc.lock().await.aux_active(),
                AuxLane::Uplink { open, .. } => open.load(Ordering::Acquire),
            };
            json!({"ok": true, "active": active})
        }
        Command::Send { frame } => {
            // The operator dead-switch is checked first (no datagram is written
            // while the aux lane is disabled by policy), and the pair must be
            // open — the stream exists only between an open and its close.
            if let Some(reply) = aux_disabled_reply(state.aux_enable) {
                return reply;
            }
            match &state.lane {
                AuxLane::Radio { proc, cfg } => {
                    if let Some(refusal) = downlink_send_refusal(&frame) {
                        return refusal;
                    }
                    if !proc.lock().await.aux_active() {
                        return json!({"ok": false, "error": "E_AUX_NOT_OPEN"});
                    }
                    // Write the validated aux-framed datagram to the local aux
                    // transmit ingress; wfb_tx radiates it and the paired node's
                    // aux-rx re-emits it to its own subscribers. A fresh socket
                    // per send (bound to an ephemeral port) is dropped after the
                    // datagram — one-shot, like open/close, with no per-send
                    // state.
                    let sock = match tokio::net::UdpSocket::bind(("127.0.0.1", 0)).await {
                        Ok(s) => s,
                        Err(e) => {
                            return json!({"ok": false, "error": format!("E_AUX_SEND_UDP: {e}")})
                        }
                    };
                    if let Err(e) = sock.send_to(&frame, ("127.0.0.1", cfg.aux_tx_port)).await {
                        return json!({"ok": false, "error": format!("E_AUX_SEND: {e}")});
                    }
                    json!({"ok": true})
                }
                AuxLane::Uplink { egress, open, .. } => {
                    if !open.load(Ordering::Acquire) {
                        return json!({"ok": false, "error": "E_AUX_NOT_OPEN"});
                    }
                    send_uplink(egress, &frame).await
                }
            }
        }
        Command::Publish { channel, payload } => state.publish(channel, payload),
    }
}

/// Why a `send` frame may not go onto a drone's aux downlink, or `None` when it
/// may.
///
/// That downlink also carries the vehicle's MAVLink, relay RPC responses and the
/// config tunnel, which the ground station trusts as coming from the drone's own
/// services. A socket client may only emit application stream traffic; anything
/// else, and any frame that does not decode, is refused rather than radiated.
pub(super) fn downlink_send_refusal(frame: &[u8]) -> Option<Value> {
    match aux_mux::decode(frame) {
        Ok((AuxChannel::AppStream, _)) => None,
        Ok(_) => Some(json!({"ok": false, "error": "E_BAD_CHANNEL"})),
        Err(e) => Some(json!({"ok": false, "error": format!("E_BAD_FRAME: {e:?}")})),
    }
}

/// Emit one `send` frame onto a ground station's shared aux uplink.
///
/// The frame is decoded before it goes anywhere: that uplink also carries the
/// drone's MAVLink, relay RPC, link feedback and config tunnel, so only the two
/// application channels may pass. A frame that does not decode (bad magic, a
/// length that disagrees with the bytes, a payload over
/// [`aux_mux::AUX_MAX_PAYLOAD`]) is refused rather than radiated as garbage.
pub(super) async fn send_uplink(egress: &AuxEgress, frame: &[u8]) -> Value {
    let (channel, payload) = match aux_mux::decode(frame) {
        Ok(decoded) => decoded,
        Err(e) => return json!({"ok": false, "error": format!("E_BAD_FRAME: {e:?}")}),
    };
    if !matches!(channel, AuxChannel::AppStream | AuxChannel::AppCommand) {
        return json!({"ok": false, "error": "E_BAD_CHANNEL"});
    }
    match egress.send(channel, payload).await {
        Ok(()) => json!({"ok": true}),
        Err(e) => json!({"ok": false, "error": format!("E_AUX_SEND: {e}")}),
    }
}

/// Inject one inbound application payload into the subscriber fan-out.
///
/// Split out from [`apply`] because it touches no radio state at all — it takes
/// the dead-switch flag and the sender rather than the whole `AuxCmdState`, so
/// the accept/refuse decision AND the actual delivery are exercisable without a
/// live `wfb_tx` process group. The channel gate is the load-bearing part: the
/// caller is a different process on the other side of a socket, and a frame from
/// any other plane on that lane must never reach a plugin's application stream.
pub(super) fn publish_app_datagram(
    aux_enable: bool,
    rx_tx: &broadcast::Sender<(u8, Vec<u8>)>,
    channel: u8,
    payload: Vec<u8>,
) -> Value {
    // Same operator dead-switch as `send` / `subscribe`: a deployment that
    // turned the aux lane off must not have application traffic arriving
    // through the back door either.
    if let Some(reply) = aux_disabled_reply(aux_enable) {
        return reply;
    }
    // Only the two APPLICATION channels. The other planes on this lane (MAVLink,
    // relay RPC, link feedback, config tunnel) have their own consumers in the
    // router process, and leaking one into the application fan-out would corrupt
    // a plugin's stream with frames it has no framing for.
    if channel != AuxChannel::AppStream as u8 && channel != AuxChannel::AppCommand as u8 {
        return json!({"ok": false, "error": "E_BAD_CHANNEL"});
    }
    // `send` returns Err only when there is no subscriber, which is the normal
    // state of a drone with no plugin attached — not a failure. The count is
    // reported so a caller can distinguish "nobody is listening" from "the lane
    // is broken" without guessing.
    let delivered = rx_tx.send((channel, payload)).unwrap_or(0);
    json!({"ok": true, "delivered": delivered})
}
