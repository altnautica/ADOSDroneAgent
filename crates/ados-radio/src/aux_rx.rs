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
use std::time::Duration;

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

/// Fixed retry between bind attempts. No backoff and no attempt cap: the aux
/// application lane is a recovery-relevant path and a node that stopped trying
/// to bind its receive half needs a human on site.
const BIND_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// Pause after a socket error before the next `recv_from`. A UDP socket in a
/// persistent error state (an ICMP-driven ECONNREFUSED storm on loopback, a
/// torn-down interface) would otherwise turn this task into a 100%-CPU spin on
/// a board that is simultaneously encoding video — thermal throttle, then
/// encoder frame drops, with the cause invisible at debug level.
const RECV_ERROR_PAUSE: Duration = Duration::from_millis(200);

/// Consecutive recv errors before the log is raised from debug to warn, so a
/// persistent fault is visible in `ados logs query` without a single transient
/// error raising an alarm.
const RECV_ERROR_WARN_AFTER: u32 = 25;

/// Bind the local aux-RX loopback port and forward decoded application frames to
/// the broadcast. Runs for the process lifetime: it is spawned once in
/// `run_service`, ABOVE the radio-group respawn loop, and owns the loopback port
/// across every bring-up.
///
/// A failed bind retries on a fixed interval rather than returning — returning
/// left the application lane permanently deaf with one error line as the only
/// trace. When no aux pair is open no datagrams arrive and the task sits idle
/// on `recv_from`.
pub async fn run_rx_loop(cfg: Arc<WfbConfig>, rx: AuxAppRx) {
    let sock = loop {
        match tokio::net::UdpSocket::bind(("127.0.0.1", cfg.aux_rx_port)).await {
            Ok(s) => break s,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    port = cfg.aux_rx_port,
                    retry_in_s = BIND_RETRY_INTERVAL.as_secs(),
                    "aux_rx_bind_failed; app datagrams not received until the bind takes"
                );
                tokio::time::sleep(BIND_RETRY_INTERVAL).await;
            }
        }
    };
    let mut buf = vec![0u8; RX_BUF_LEN];
    let mut recv_errors: u32 = 0;
    loop {
        let n = match sock.recv_from(&mut buf).await {
            Ok((n, _)) => {
                recv_errors = 0;
                n
            }
            Err(e) => {
                // A transient socket error must not kill the loop: pause, then
                // retry. The pause is what keeps a persistently erroring socket
                // from burning a core.
                recv_errors = recv_errors.saturating_add(1);
                if recv_errors >= RECV_ERROR_WARN_AFTER
                    && recv_errors.is_multiple_of(RECV_ERROR_WARN_AFTER)
                {
                    tracing::warn!(
                        error = %e,
                        consecutive = recv_errors,
                        "aux_rx_recv_error_persistent"
                    );
                } else {
                    tracing::debug!(error = %e, "aux_rx_recv_error");
                }
                tokio::time::sleep(RECV_ERROR_PAUSE).await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WfbConfig;

    /// An unused loopback UDP port, so concurrent tests never collide on the
    /// shipped default.
    fn free_port() -> u16 {
        let probe = std::net::UdpSocket::bind("127.0.0.1:0").expect("probe bind");
        probe.local_addr().expect("probe addr").port()
    }

    #[tokio::test]
    async fn an_app_datagram_after_a_respawn_still_reaches_a_subscriber() {
        // RADIO-AUXRX-RESPAWN-LEAK regression.
        //
        // The fan-out channel and this receive loop are process-lifetime state
        // created ABOVE `run_service`'s respawn loop. When they were created
        // INSIDE it, the first radio-group respawn (a wfb_tx stall kill, an
        // FEC/MCS retune crash, a hop end — all routine) left generation 1's
        // task owning the loopback port and publishing into a channel with no
        // subscribers, while generation 2's subscribers attached to a fresh
        // channel nothing fed and generation 2's own receive task died on
        // EADDRINUSE. Every inbound auxiliary application datagram was lost for
        // the rest of the process lifetime.
        let port = free_port();
        let cfg = WfbConfig {
            aux_rx_port: port,
            ..WfbConfig::default()
        };
        let (aux_app_tx, _hoisted_keepalive) = broadcast::channel::<(u8, Vec<u8>)>(256);
        tokio::spawn(run_rx_loop(Arc::new(cfg), aux_app_tx.clone()));

        // Generation 1 bring-up: the aux command socket's state is built from
        // the hoisted sender and a plugin subscribes; then the radio group is
        // killed and generation 1's per-bring-up state is dropped.
        let gen1_rx_tx = aux_app_tx.clone();
        let gen1_subscriber = gen1_rx_tx.subscribe();
        drop(gen1_subscriber);
        drop(gen1_rx_tx);

        // Generation 2 bring-up: state rebuilt from the same hoisted sender.
        let gen2_rx_tx = aux_app_tx.clone();
        let mut gen2_subscriber = gen2_rx_tx.subscribe();

        // A datagram arriving AFTER the respawn must reach generation 2's
        // subscriber. Re-sent on a short cadence because the bind races the
        // spawn and UDP to an unbound port is dropped, not queued.
        let frame = aux_mux::encode(AuxChannel::AppCommand, b"pong").expect("encode");
        let sender = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("sender bind");
        let got = loop {
            sender
                .send_to(&frame, ("127.0.0.1", port))
                .await
                .expect("send");
            match tokio::time::timeout(Duration::from_millis(100), gen2_subscriber.recv()).await {
                Ok(res) => break res.expect("the hoisted channel must stay open"),
                Err(_) => continue,
            }
        };
        assert_eq!(
            got,
            (AuxChannel::AppCommand as u8, b"pong".to_vec()),
            "a post-respawn subscriber must receive inbound app datagrams"
        );
    }

    #[tokio::test]
    async fn a_bind_collision_retries_instead_of_giving_up() {
        // A transient EADDRINUSE must self-heal on a fixed retry: returning left
        // the application lane permanently deaf with one error line as the only
        // trace.
        let port = free_port();
        let cfg = WfbConfig {
            aux_rx_port: port,
            ..WfbConfig::default()
        };

        // Hold the port so the first bind attempts fail.
        let squatter = tokio::net::UdpSocket::bind(("127.0.0.1", port))
            .await
            .expect("squatter bind");
        let (aux_app_tx, mut sub) = broadcast::channel::<(u8, Vec<u8>)>(256);
        tokio::spawn(run_rx_loop(Arc::new(cfg), aux_app_tx));
        // Let at least one bind attempt fail, then release the port.
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(squatter);

        let frame = aux_mux::encode(AuxChannel::AppStream, b"late").expect("encode");
        let sender = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("sender bind");
        // Well past one BIND_RETRY_INTERVAL: the loop must have re-bound.
        let deadline = tokio::time::Instant::now() + BIND_RETRY_INTERVAL * 3;
        let got = loop {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the bind never retried after the port was released"
            );
            sender
                .send_to(&frame, ("127.0.0.1", port))
                .await
                .expect("send");
            match tokio::time::timeout(Duration::from_millis(200), sub.recv()).await {
                Ok(res) => break res.expect("channel open"),
                Err(_) => continue,
            }
        };
        assert_eq!(got, (AuxChannel::AppStream as u8, b"late".to_vec()));
    }
}
