//! UDP fan-out for the ground-side video stream.
//!
//! The receive-side wfb decoder outputs the FEC-decoded RTP H.264 stream to a
//! single internal UDP port. Two consumers want to read it:
//!
//! 1. The mediamtx-gs media server's native `udp+rtp` source — for the
//!    browser WHEP stream.
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
//! beyond what the kernel UDP socket buffer enforces — which is why both
//! sockets ask the kernel for a 4 MB buffer instead of running on the ~208 KB
//! default that sheds packets the moment a consumer stalls.
//!
//! The forwarded and drop totals are supervised rather than merely published: a
//! generation that has forwarded at least one datagram and then goes flat for
//! [`FANOUT_STALL_WINDOW`] is rebound, and a sustained climb in send failures is
//! reported as its own downstream degradation. A relay whose counter is frozen
//! is doing no work, so its own liveness proves nothing.
//!
//! The receive side of that loop is resilient: a transient `recv_from` error
//! (an ICMP port-unreachable surfacing as a connreset on the local socket, a
//! momentary EINTR/EAGAIN) is logged and the loop continues rather than ending
//! the task. If the recv loop returned on the first transient error the
//! generation's video forwarding (decoder → mediamtx + LCD) would silently stop
//! while the data RX itself kept running, so the loop only exits on a fatal
//! socket condition that cannot recover in place.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UdpSocket;

mod repoint;
mod watch;
pub use repoint::{
    resolve_fanout_slot, run_default_fanout, run_repointing_fanout, HERO_POLL_INTERVAL,
};
pub use watch::fanout_decision;
use watch::watch_for_fanout_stall;

/// Downstream port for the mediamtx-gs native `udp+rtp` source.
pub const MEDIAMTX_PORT: u16 = 5600;
/// Downstream port for the on-device LCD video tap.
pub const LCD_PORT: u16 = 5605;

/// Shared cumulative fan-out counters, cloneable across the tasks that need to
/// read them.
///
/// The fan-out sits BETWEEN the wfb_rx decode and the mediamtx-gs source — a hop
/// that was otherwise blind to the cross-process diagnostics. The stats reader
/// folds these totals onto the `wfb-stats.json` sidecar so the video-pipeline
/// harness can confirm the fan-out is actually forwarding (decoded packets
/// climbing but `fanout_forwarded` flat isolates the fault to the fan-out; both
/// climbing but the media server's bytesReceived flat isolates it to mediamtx-gs).
/// `forwarded` counts datagrams read from the decoder; `drops` counts per-target
/// `send_to` failures.
#[derive(Clone, Default)]
pub struct FanoutCounters {
    forwarded: Arc<AtomicU64>,
    drops: Arc<AtomicU64>,
}

impl FanoutCounters {
    /// A fresh zeroed counter set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Cumulative datagrams read from the decoder and forwarded.
    pub fn forwarded(&self) -> u64 {
        self.forwarded.load(Ordering::Relaxed)
    }

    /// Cumulative per-target `send_to` failures.
    pub fn drops(&self) -> u64 {
        self.drops.load(Ordering::Relaxed)
    }
}

/// Max datagram size we are willing to read in one go. RTP packets over the
/// 5 GHz video link sit well under this; the headroom covers jumbo edge cases.
const BUF_SIZE: usize = 65536;

/// Receive/send buffer size asked of the kernel for the two fan-out sockets.
///
/// This hop carries EVERY ground-side video packet, and the kernel default
/// (~208 KB on Linux) drops datagrams as soon as a consumer stalls even
/// briefly, so both sockets ask for 4 MB. Without that `setsockopt` the video
/// hop runs on the default. The loss happens BELOW the application: a datagram
/// the kernel had nowhere to put never reaches `recv_from`, so neither
/// `recv_errors` nor `drops` ever sees it, and the only symptom is corrupt
/// video. 4 MB covers the 5 GHz stream's burst size while the single read loop
/// is mid-`send_to`.
const FANOUT_SOCKET_BUFFER_BYTES: usize = 4 * 1024 * 1024;

/// Ask the kernel for a larger receive buffer on the decoder-side socket and
/// log what it actually granted.
///
/// Linux silently doubles the request (its own bookkeeping overhead) and caps
/// it at `net.core.rmem_max`, so a readback never equals the request; the
/// obtained value is logged rather than asserted. Failure is non-fatal and must
/// stay that way: forwarding with the default buffer is a small buffer, while
/// failing the bind over a refused socket option is going dark.
fn set_recv_buffer(sock: &UdpSocket) {
    let opts = socket2::SockRef::from(sock);
    if let Err(e) = opts.set_recv_buffer_size(FANOUT_SOCKET_BUFFER_BYTES) {
        tracing::warn!(
            error = %e,
            requested = FANOUT_SOCKET_BUFFER_BYTES,
            "fanout_rcvbuf_set_failed"
        );
        return;
    }
    match opts.recv_buffer_size() {
        Ok(actual) => tracing::info!(
            requested = FANOUT_SOCKET_BUFFER_BYTES,
            actual,
            "fanout_rcvbuf"
        ),
        Err(e) => tracing::warn!(error = %e, "fanout_rcvbuf_read_failed"),
    }
}

/// Ask the kernel for a larger send buffer on the relay socket, so it can queue
/// a burst of forwarded packets instead of blocking the recv loop. Same clamp
/// and same non-fatal contract as [`set_recv_buffer`].
fn set_send_buffer(sock: &UdpSocket) {
    let opts = socket2::SockRef::from(sock);
    if let Err(e) = opts.set_send_buffer_size(FANOUT_SOCKET_BUFFER_BYTES) {
        tracing::warn!(
            error = %e,
            requested = FANOUT_SOCKET_BUFFER_BYTES,
            "fanout_sndbuf_set_failed"
        );
        return;
    }
    match opts.send_buffer_size() {
        Ok(actual) => tracing::info!(
            requested = FANOUT_SOCKET_BUFFER_BYTES,
            actual,
            "fanout_sndbuf"
        ),
        Err(e) => tracing::warn!(error = %e, "fanout_sndbuf_read_failed"),
    }
}

/// Run the fan-out forever: forward every datagram from `listen_addr` to each
/// address in `targets`. Returns only on a fatal socket error or when the
/// future is dropped (cancellation). The caller supervises lifecycle and
/// restart; this loop does not implement its own retry.
///
/// `counters` accumulates the forwarded/drop totals so a cross-process reader
/// (the stats reader → sidecar → diagnostics harness) can see the fan-out hop.
pub async fn run_fanout(
    listen_addr: SocketAddr,
    targets: &[SocketAddr],
    counters: &FanoutCounters,
) -> std::io::Result<()> {
    if targets.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "no fanout targets configured",
        ));
    }

    let in_sock = UdpSocket::bind(listen_addr).await?;
    set_recv_buffer(&in_sock);
    // One output socket for all destinations. Bind to the unspecified address
    // so the kernel picks an ephemeral source port; we only ever `send_to`.
    let out_sock = UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, 0)).await?;
    set_send_buffer(&out_sock);

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
                counters.drops.fetch_add(1, Ordering::Relaxed);
            }
        }
        forwarded += 1;
        // Publish the cumulative forwarded total for the cross-process reader
        // (the wfb-stats sidecar) so the fan-out hop is no longer blind.
        counters.forwarded.fetch_add(1, Ordering::Relaxed);
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

/// How long the supervisor waits before rebinding after the inner loop returned
/// a fatal socket error. Bounded and non-zero: a port that is momentarily
/// unbindable (the previous generation's socket still closing) clears in well
/// under this, and a permanently unbindable one must not spin the reactor.
const REBIND_BACKOFF: Duration = Duration::from_millis(500);

/// How long the generation's forwarded count must sit flat before the fan-out
/// is judged stalled.
///
/// Chosen against this hop's real cadence, not copied: the fan-out relays
/// FEC-decoded RTP video at tens of Mbps in ~1.4 KB datagrams, so `forwarded`
/// advances several thousand times a second and the normal gap between two
/// advances is well under a millisecond. Flat for three seconds is therefore
/// four orders of magnitude past the inter-datagram gap — thousands of missing
/// datagrams, unambiguously not jitter.
///
/// The window is deliberately not set at the edge of that measurement. Decoded
/// output can legitimately pause for a beat (a channel re-acquisition, an FEC
/// block waiting on its last fragment, a mid-flight hero re-point), and
/// rebinding on an ordinary radio hiccup would cost more video than it saves.
/// Three seconds is far enough above the hiccup and still below the point where
/// an operator staring at a frozen picture would rather the socket had been
/// rebound.
pub const FANOUT_STALL_WINDOW: Duration = Duration::from_secs(3);

/// How often the watchdog samples the counters — several times per window, so a
/// stall is caught at the window edge rather than a whole window late.
const FANOUT_SAMPLE_INTERVAL: Duration = Duration::from_millis(500);

/// Per-target `send_to` failures within one [`FANOUT_STALL_WINDOW`] that mark
/// the downstream as not draining.
///
/// A consumer that is restarting produces a handful of failures as its socket
/// goes away and comes back; a consumer that is genuinely not draining fails at
/// the stream's own rate, i.e. thousands per second. A hundred failures inside
/// one window sits between the two with room to spare in both directions.
pub const FANOUT_DROP_DEGRADED_DELTA: u64 = 100;

/// What the forward path is doing across a pair of counter samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForwardState {
    /// Nothing has been forwarded since this socket was bound, so there is
    /// nothing to forward: an idle ground station with no drone paired, or one
    /// whose drone has powered off. NOT a fault, and the gate that keeps the
    /// stall check from ever tripping before the first datagram — the same
    /// `output_seen` gate the wfb tap's output watchdog uses.
    AwaitingFirstDatagram,
    /// The counter advanced since the previous sample — healthy.
    Advancing { datagrams: u64 },
    /// Flat, but still inside the stall window — healthy.
    WithinStallWindow,
    /// Flat for the whole window after at least one datagram was forwarded:
    /// the real fault. Carries the frozen count so the log names it.
    Stalled { frozen_at: u64 },
}

/// Whether the downstream consumers are draining what the fan-out sends them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainState {
    /// No sustained send failure over the window.
    Draining,
    /// `send_to` kept failing across the window: a downstream consumer is not
    /// draining its socket. Reported separately from a flat `forwarded` on
    /// purpose — here the fan-out is reading and relaying correctly and the far
    /// end is what is behind, so rebinding this socket would fix nothing.
    NotDraining { drops: u64 },
}

/// The fan-out watchdog verdict over one counter sample.
///
/// The two axes are independent: a fan-out can be forwarding perfectly into a
/// consumer that has stopped draining, and a stalled fan-out drops nothing at
/// all because it has nothing to send. Collapsing them into one health flag
/// would report the wrong fault in both directions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FanoutDecision {
    /// The forward path's state.
    pub forward: ForwardState,
    /// The downstream drain state.
    pub drain: DrainState,
}

#[cfg(test)]
mod tests;
