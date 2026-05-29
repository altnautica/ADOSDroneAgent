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

    loop {
        let (len, _addr) = in_sock.recv_from(&mut buf).await?;
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
        // Periodic counter log so a long-run drift in drop rate is visible
        // without flooding the journal.
        if forwarded.is_multiple_of(5000) {
            tracing::info!(forwarded, drops, "fanout_progress");
        }
    }
}

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
}
