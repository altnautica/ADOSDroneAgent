//! UDP fan-out for the ground-side video stream.
//!
//! The receive-side wfb decoder outputs the FEC-decoded RTP H.264 stream to a
//! single internal UDP port. Two consumers want to read it:
//!
//! 1. The mediamtx-gs ffmpeg ingest sidecar — for the browser WHEP stream.
//! 2. The on-device LCD video tap — for the local screen.
//!
//! Only one process can bind a UDP port at a time, and `SO_REUSEPORT`
//! load-balances rather than duplicating, so a tiny fan-out reads each datagram
//! from the decoder's output port and re-emits it to both downstream localhost
//! ports. Per-packet relay cost is sub-millisecond.
//!
//! The fan-out is a stateless RTP forwarder (viewers read UDP directly).
//! Datagrams are RTP packets; we don't parse them, just copy + send. A single
//! `recv_from`/`send_to` loop with no queueing, reordering, or drop policy
//! beyond what the kernel UDP socket buffer enforces. A drop counter on send
//! failure surfaces in the log so the supervisor captures sustained problems.
//!
//! The receive side of that loop is resilient: a transient `recv_from` error
//! (an ICMP port-unreachable surfacing as a connreset on the local socket, a
//! momentary EINTR/EAGAIN) is logged and the loop continues rather than ending
//! the task. If the recv loop returned on the first transient error the
//! generation's video forwarding (decoder → mediamtx + LCD) would silently stop
//! while the data RX itself kept running, so the loop only exits on a fatal
//! socket condition that cannot recover in place.

use std::net::SocketAddr;

use tokio::net::UdpSocket;

/// Internal listen port: the wfb decoder emits here, the fan-out reads here.
pub const INTERNAL_LISTEN_PORT: u16 = 5599;
/// Downstream port for the mediamtx-gs ffmpeg ingest.
pub const MEDIAMTX_PORT: u16 = 5600;
/// Downstream port for the on-device LCD video tap.
pub const LCD_PORT: u16 = 5605;

/// Max datagram size we are willing to read in one go. RTP packets over the
/// 5 GHz video link sit well under this; the headroom covers jumbo edge cases.
const BUF_SIZE: usize = 65536;

/// Run the fan-out forever: forward every datagram from `listen_addr` to each
/// address in `targets`. Returns only on a fatal socket error or when the
/// future is dropped (cancellation). The caller supervises lifecycle and
/// restart; this loop does not implement its own retry.
pub async fn run_fanout(listen_addr: SocketAddr, targets: &[SocketAddr]) -> std::io::Result<()> {
    if targets.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "no fanout targets configured",
        ));
    }

    let in_sock = UdpSocket::bind(listen_addr).await?;
    // One output socket for all destinations. Bind to the unspecified address
    // so the kernel picks an ephemeral source port; we only ever `send_to`.
    let out_sock = UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, 0)).await?;

    tracing::info!(
        listen = %listen_addr,
        targets = ?targets,
        "fanout_started"
    );

    let mut buf = vec![0u8; BUF_SIZE];
    let mut forwarded: u64 = 0;
    let mut drops: u64 = 0;
    let mut recv_errors: u64 = 0;
    // Consecutive recv errors with no intervening successful read. A datagram
    // sent to a downstream consumer that is not listening can come back as a
    // socket error on the next recv; that is transient and self-clears. A run of
    // errors with zero successes means the socket cannot be read from at all, so
    // a short sleep keeps a hard failure from spinning the CPU at 100% while the
    // generation supervisor decides the data RX is gone.
    let mut consecutive_errors: u32 = 0;

    loop {
        let (len, _addr) = match in_sock.recv_from(&mut buf).await {
            Ok(v) => {
                consecutive_errors = 0;
                v
            }
            Err(e) => {
                // A recv error must not end the loop: that would silently stop
                // video forwarding for the whole generation. Log it, count it,
                // and read again. Back off briefly only on a sustained run so a
                // wedged socket cannot busy-spin.
                recv_errors += 1;
                consecutive_errors = consecutive_errors.saturating_add(1);
                tracing::warn!(
                    error = %e,
                    recv_errors,
                    consecutive_errors,
                    "fanout_recv_error"
                );
                if consecutive_errors >= ERROR_BACKOFF_THRESHOLD {
                    tokio::time::sleep(ERROR_BACKOFF).await;
                }
                continue;
            }
        };
        if len == 0 {
            continue;
        }
        let payload = &buf[..len];
        for target in targets {
            // A send failure to one target (e.g. consumer not yet up) must not
            // stall the other; count it and carry on. The kernel UDP buffer is
            // the only backpressure.
            if out_sock.send_to(payload, target).await.is_err() {
                drops += 1;
            }
        }
        forwarded += 1;
        // Periodic counter log so a long-run drift in drop or recv-error rate is
        // visible without flooding the journal.
        if forwarded.is_multiple_of(5000) {
            tracing::info!(forwarded, drops, recv_errors, "fanout_progress");
        }
    }
}

/// Consecutive recv errors before the loop inserts a short backoff sleep. Below
/// this a transient error (e.g. a connreset from a downstream consumer that is
/// not yet up) is retried immediately so video forwarding is never delayed.
const ERROR_BACKOFF_THRESHOLD: u32 = 8;

/// Backoff applied once recv errors stop self-clearing, so a hard socket failure
/// cannot busy-spin the reactor while the generation supervisor tears the
/// receive plane down and respawns it.
const ERROR_BACKOFF: std::time::Duration = std::time::Duration::from_millis(100);

/// The default ground-station fan-out wiring: listen on the internal port,
/// forward to the mediamtx ingest and the LCD tap, all on localhost.
pub async fn run_default_fanout() -> std::io::Result<()> {
    let listen: SocketAddr = (std::net::Ipv4Addr::LOCALHOST, INTERNAL_LISTEN_PORT).into();
    let targets = [
        (std::net::Ipv4Addr::LOCALHOST, MEDIAMTX_PORT).into(),
        (std::net::Ipv4Addr::LOCALHOST, LCD_PORT).into(),
    ];
    run_fanout(listen, &targets).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn fan_out_delivers_each_datagram_to_both_targets() {
        // Bind the two downstream consumers on ephemeral ports first so we know
        // their addresses for the targets list.
        let mediamtx = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let lcd = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let mediamtx_addr = mediamtx.local_addr().unwrap();
        let lcd_addr = lcd.local_addr().unwrap();

        // Listen on an ephemeral port (stand-in for 5599) so the test never
        // collides with a real fan-out or another parallel test.
        let listen = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let listen_addr = listen.local_addr().unwrap();
        drop(listen); // free it for run_fanout to bind.

        let targets = [mediamtx_addr, lcd_addr];
        let fanout = tokio::spawn(async move {
            // Ignore the never-Ok result; the task is aborted at test end.
            let _ = run_fanout(listen_addr, &targets).await;
        });

        // UDP gives no delivery guarantee and the fan-out's bind may land after
        // our first send, so resend on a short cadence until each consumer has
        // seen the payload. The fan-out spawn task is racing us; a handful of
        // datagrams is plenty.
        let sender = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let payload = b"the-rtp-payload";
        let resend = async {
            loop {
                let _ = sender.send_to(payload, listen_addr).await;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };

        // Both consumers should receive the datagram while the resender runs.
        let recv_both = async {
            let mut mbuf = [0u8; 64];
            let mut lbuf = [0u8; 64];
            let mlen = mediamtx.recv(&mut mbuf).await.unwrap();
            assert_eq!(&mbuf[..mlen], payload);
            let llen = lcd.recv(&mut lbuf).await.unwrap();
            assert_eq!(&lbuf[..llen], payload);
        };

        tokio::select! {
            _ = resend => unreachable!("resender never returns"),
            res = tokio::time::timeout(Duration::from_secs(5), recv_both) => {
                res.expect("consumers timed out waiting for fan-out delivery");
            }
        }

        fanout.abort();
    }

    #[tokio::test]
    async fn empty_targets_is_rejected() {
        let listen: SocketAddr = (std::net::Ipv4Addr::LOCALHOST, 0).into();
        let err = run_fanout(listen, &[]).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn error_backoff_is_bounded_and_non_zero() {
        // The backoff guard must be a small, non-zero, finite sleep so a wedged
        // recv socket neither busy-spins (zero) nor stalls a recoverable
        // generation (too long). It only engages after a sustained error run.
        const { assert!(ERROR_BACKOFF_THRESHOLD >= 1) };
        assert!(!ERROR_BACKOFF.is_zero());
        assert!(ERROR_BACKOFF <= Duration::from_secs(1));
    }

    #[tokio::test]
    async fn fan_out_survives_a_target_that_starts_down_then_comes_up() {
        // A downstream consumer that is not yet listening can make a `send_to`
        // fail and, on the next read, surface a recv error on the local socket.
        // The loop must NOT end on that: it must keep forwarding so that once the
        // consumer comes up it receives the stream. This proves the recv loop is
        // resilient rather than dying on the first transient error.

        // Bind one live consumer up front; leave the second target pointed at a
        // port we deliberately do not bind until partway through.
        let live = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let live_addr = live.local_addr().unwrap();

        // Reserve, then free, a port for the "down" consumer so we know its
        // address but nothing is listening there at first.
        let down_reserved = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let down_addr = down_reserved.local_addr().unwrap();
        drop(down_reserved);

        let listen = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let listen_addr = listen.local_addr().unwrap();
        drop(listen);

        let targets = [live_addr, down_addr];
        let fanout = tokio::spawn(async move {
            let _ = run_fanout(listen_addr, &targets).await;
        });

        let sender = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let payload = b"the-rtp-payload";

        // Drive traffic while the down target is closed; the live consumer keeps
        // receiving regardless, proving the loop did not die.
        let resend = async {
            loop {
                let _ = sender.send_to(payload, listen_addr).await;
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
        };

        let live_keeps_receiving = async {
            let mut buf = [0u8; 64];
            // Receive several datagrams across the window where the other target
            // is down, then bind that target and confirm it starts receiving too.
            for _ in 0..3 {
                let n = live.recv(&mut buf).await.unwrap();
                assert_eq!(&buf[..n], payload);
            }
            // Now bring the previously-down consumer up and confirm the still-alive
            // loop delivers to it.
            let down = UdpSocket::bind(down_addr).await.unwrap();
            let n = tokio::time::timeout(Duration::from_secs(3), down.recv(&mut buf))
                .await
                .expect("recovered target never received after coming up")
                .unwrap();
            assert_eq!(&buf[..n], payload);
        };

        tokio::select! {
            _ = resend => unreachable!("resender never returns"),
            res = tokio::time::timeout(Duration::from_secs(6), live_keeps_receiving) => {
                res.expect("fan-out stopped forwarding to the live consumer");
            }
        }

        fanout.abort();
    }
}
