//! Auxiliary-stream application receive loop.
//!
//! When the aux pair is open, the transmitting peer radiates application
//! datagrams over the link and the local `wfb_rx` re-emits each decoded frame on
//! `127.0.0.1:<cfg.aux_rx_port>` (see [`crate::process::aux_rx_args`]). Before
//! this task existed nothing read that loopback port, so an open aux pair could
//! transmit but never deliver inbound application frames. This task owns the
//! local receive half: it binds the loopback port for the process lifetime,
//! decodes each datagram as an aux frame, and fans out the application channels
//! (`AppStream` / `AppCommand`) to every aux-subscribe connection via the shared
//! broadcast sender.
//!
//! Any other decodable channel (MAVLink, status, config-tunnel, ...) is dropped:
//! those planes are handled by their own consumers and must not leak into the
//! application lane. A foreign / malformed datagram is dropped (counted by a
//! `tracing::debug!`, no alarm) so a noisy shared adapter cannot crash the loop.

use std::sync::Arc;

use ados_protocol::aux_mux::{self, AuxChannel};
use tokio::sync::broadcast;

use crate::config::WfbConfig;

/// The buffer a single aux frame (plus the UDP overhead headroom) can occupy.
/// [`ados_protocol::aux_mux::AUX_MAX_PAYLOAD`] is the largest payload; the frame
/// is header + payload, and we allow slack so a slightly-oversized datagram is
/// read in full and rejected by the decoder rather than being silently trimmed.
const RX_BUF_LEN: usize =
    ados_protocol::aux_mux::AUX_MAX_PAYLOAD + ados_protocol::aux_mux::AUX_HEADER_LEN + 64;

/// Broadcast a decoded application datagram to every aux-subscribe subscriber.
/// Each item is `(channel as u8, payload)` where `channel` is `AppStream` (8) or
/// `AppCommand` (9); the payload is the application bytes, not the full aux
/// frame.
type AuxAppRx = broadcast::Sender<(u8, Vec<u8>)>;

/// Bind the local aux-RX loopback port and forward decoded application frames to
/// the broadcast until the socket errors. Runs for the process lifetime (spawned
/// once at bring-up alongside the aux command socket); when no aux pair is open
/// no datagrams arrive and the task sits idle on `recv_from`.
pub async fn run_rx_loop(cfg: Arc<WfbConfig>, rx: AuxAppRx) {
    let sock = match tokio::net::UdpSocket::bind(("127.0.0.1", cfg.aux_rx_port)).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(
                error = %e,
                port = cfg.aux_rx_port,
                "aux_rx_bind_failed; app datagrams will not be received"
            );
            return;
        }
    };
    let mut buf = vec![0u8; RX_BUF_LEN];
    loop {
        let n = match sock.recv_from(&mut buf).await {
            Ok((n, _)) => n,
            Err(e) => {
                // A transient socket error must not kill the loop (mirrors the
                // command-socket accept backoff philosophy): log and retry.
                tracing::debug!(error = %e, "aux_rx_recv_error");
                continue;
            }
        };
        match aux_mux::decode(&buf[..n]) {
            Ok((channel, payload)) => match channel {
                AuxChannel::AppStream | AuxChannel::AppCommand => {
                    let _ = rx.send((channel as u8, payload.to_vec()));
                }
                // A well-framed frame on a plane this task does not own (MAVLink,
                // status, config tunnel, ...). Those consumers handle it; leaking
                // it into the application lane would corrupt the app stream.
                other => {
                    tracing::debug!(channel = other as u8, "aux_rx_dropped_other_channel");
                }
            },
            // Malformed / foreign bytes on the port. No alarm: a shared adapter
            // legitimately carries non-aux traffic, and a truncated frame of ours
            // is a transport fault best surfaced by the link-side watchdogs.
            Err(e) => {
                tracing::debug!(error = ?e, "aux_rx_decode_dropped");
            }
        }
    }
}
