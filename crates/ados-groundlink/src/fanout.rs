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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UdpSocket;

/// Downstream port for the mediamtx-gs ffmpeg ingest.
pub const MEDIAMTX_PORT: u16 = 5600;
/// Downstream port for the on-device LCD video tap.
pub const LCD_PORT: u16 = 5605;

/// Shared cumulative fan-out counters, cloneable across the tasks that need to
/// read them.
///
/// The fan-out sits BETWEEN the wfb_rx decode and the mediamtx-gs ingest — a hop
/// that was otherwise blind to the cross-process diagnostics. The stats reader
/// folds these totals onto the `wfb-stats.json` sidecar so the video-pipeline
/// harness can confirm the fan-out is actually forwarding (decoded packets
/// climbing but `fanout_forwarded` flat isolates the fault to the fan-out; both
/// climbing but the ingest byte-rate flat isolates it to the ingest ffmpeg).
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

/// How often the fan-out re-reads the published hero selection.
///
/// Slow on purpose: the read costs a tmpfs stat in the common case (no
/// selection published, single-drone fleet) and the operator-visible cost of a
/// second's delay between clicking a drone and its video arriving is nothing
/// next to the encoder respawn that switch already waits on.
pub const HERO_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// How long the supervisor waits before rebinding after the inner loop returned
/// a fatal socket error. Bounded and non-zero: a port that is momentarily
/// unbindable (the previous generation's socket still closing) clears in well
/// under this, and a permanently unbindable one must not spin the reactor.
const REBIND_BACKOFF: Duration = Duration::from_millis(500);

/// Which fleet slot the fan-out should be serving right now.
///
/// The published hero selection wins when it is live — the sidecar names a slot
/// AND the device that holds it, and the fleet registry still agrees. Anything
/// else falls back to `fallback_slot`, the generation's primary (the lowest
/// registered slot), which is the whole behaviour before hero selection existed
/// and remains correct for the single-drone fleet that never publishes one.
///
/// Requiring the registry to agree is what makes a stale selection harmless: a
/// hero that has since unpaired no longer matches its slot, so the fan-out
/// returns to the primary instead of listening forever on a port that nothing
/// transmits to.
pub fn resolve_fanout_slot(fallback_slot: u8, hero_path: &Path, registry_path: &Path) -> u8 {
    let Some(hero) = crate::fleet_hero::read_hero_from(hero_path) else {
        return fallback_slot;
    };
    let still_registered = crate::fleet::FleetRegistry::load(registry_path)
        .slots()
        .any(|s| s.slot == hero.slot && s.device_id == hero.device_id);
    if still_registered {
        hero.slot
    } else {
        fallback_slot
    }
}

/// Run the fan-out, re-pointing it whenever the slot it should serve changes.
///
/// The inner [`run_fanout`] loop owns one bound socket, so following a new hero
/// means binding a different port — and rebinding is the ONLY thing that
/// happens on a change: while the answer from `resolve` is unchanged the loop is
/// never touched, so a fleet with a settled hero forwards every datagram without
/// interruption. `resolve` returning the same slot forever (the absent-sidecar
/// case) therefore costs one poll per interval and no rebind at all.
///
/// Returns only when `resolve` cannot be served at all is not a case: a fatal
/// socket error backs off and re-resolves rather than ending the fan-out for the
/// generation, because going permanently dark is a worse answer than retrying a
/// bind that is very likely transient.
pub async fn run_repointing_fanout<R, P>(
    resolve: R,
    port_for: P,
    targets: &[SocketAddr],
    counters: &FanoutCounters,
    poll: Duration,
) -> std::io::Result<()>
where
    R: Fn() -> u8,
    P: Fn(u8) -> u16,
{
    loop {
        let slot = resolve();
        let listen: SocketAddr = (std::net::Ipv4Addr::LOCALHOST, port_for(slot)).into();
        tokio::select! {
            res = run_fanout(listen, targets, counters) => {
                match res {
                    // `run_fanout` does not return on success; treat it as a
                    // clean stop rather than inventing a reason to loop.
                    Ok(()) => return Ok(()),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            slot,
                            listen = %listen,
                            "fanout_socket_failed_rebinding"
                        );
                        tokio::time::sleep(REBIND_BACKOFF).await;
                    }
                }
            }
            next = wait_for_slot_change(&resolve, slot, poll) => {
                tracing::info!(from = slot, to = next, "fanout_repointed_to_hero_slot");
            }
        }
    }
}

/// Resolve on a slow tick until the answer differs from `current`, then return
/// the new slot. Never returns while the selection is unchanged, which is what
/// leaves the running fan-out undisturbed.
async fn wait_for_slot_change<R: Fn() -> u8>(resolve: &R, current: u8, poll: Duration) -> u8 {
    loop {
        tokio::time::sleep(poll).await;
        let next = resolve();
        if next != current {
            return next;
        }
    }
}

/// The default ground-station fan-out wiring: listen on the hero's video egress,
/// forward to the mediamtx ingest and the LCD tap, all on localhost.
/// `fallback_slot` is the generation's primary, served whenever no live hero
/// selection is published. `counters` is the shared handle the stats reader also
/// holds so the sidecar surfaces the forwarded/drop totals.
///
/// The two downstream ports are single-stream surfaces (one mediamtx ingest, one
/// LCD tap), so exactly one slot is fanned out at a time: the operator's hero.
/// Every other registered slot is still fully received and FEC-decoded onto
/// `VIDEO_RX_PORT_BASE + slot`, but nothing reads those ports — no per-drone
/// ingest exists yet — so a non-hero drone's video is decoded and discarded.
/// That is what makes serving the RIGHT slot here the whole of what the operator
/// sees.
pub async fn run_default_fanout(
    fallback_slot: u8,
    counters: FanoutCounters,
) -> std::io::Result<()> {
    let hero_path = PathBuf::from(crate::fleet_hero::hero_path());
    let registry_path = PathBuf::from(crate::fleet::FLEET_REGISTRY_PATH);
    let targets = [
        (std::net::Ipv4Addr::LOCALHOST, MEDIAMTX_PORT).into(),
        (std::net::Ipv4Addr::LOCALHOST, LCD_PORT).into(),
    ];
    run_repointing_fanout(
        || resolve_fanout_slot(fallback_slot, &hero_path, &registry_path),
        crate::wfb_rx::video_rx_port,
        &targets,
        &counters,
        HERO_POLL_INTERVAL,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::FleetRegistry;
    use std::time::Duration;

    /// A registry file holding `ids` on slots 1..=n, and the temp dir that owns
    /// it. `allocate` issues the lowest free slot first, so `ids[0]` lands on
    /// slot 1.
    fn registry_with(dir: &Path, ids: &[&str]) -> PathBuf {
        let path = dir.join("fleet.json");
        let mut reg = FleetRegistry::default();
        for id in ids {
            reg.allocate(id).expect("a slot must be issuable");
        }
        reg.persist(&path).unwrap();
        path
    }

    /// Reserve an ephemeral UDP port and give it straight back, so the caller
    /// knows a free address without holding it.
    async fn free_port() -> u16 {
        let s = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = s.local_addr().unwrap().port();
        drop(s);
        port
    }

    /// Drive `payload` at `port` on a tight cadence until dropped. UDP gives no
    /// delivery guarantee and the fan-out's bind races the first send, so every
    /// test that asserts on delivery resends.
    async fn drive(port: u16, payload: &'static [u8]) {
        let sender = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let addr: SocketAddr = (std::net::Ipv4Addr::LOCALHOST, port).into();
        loop {
            let _ = sender.send_to(payload, addr).await;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn the_fan_out_forwards_the_hero_slot_not_the_lowest_registered_one() {
        // The operator-visible bug: with two drones registered, selecting the
        // one on slot 2 as hero promoted it to full video while the ground
        // station kept forwarding slot 1 — a different aircraft, and by then a
        // 1 fps thumbnail. Both slots transmit here, and the assertion is on
        // WHICH stream reaches the downstream consumers.
        let dir = tempfile::tempdir().unwrap();
        let registry_path = registry_with(dir.path(), &["drone-a", "drone-b"]);
        let hero_path = dir.path().join("fleet-hero.json");
        crate::fleet_hero::write_hero_to(&hero_path, 2, "drone-b").unwrap();

        // Stand-ins for the mediamtx ingest and the LCD tap.
        let consumer = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let lcd = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let targets = [consumer.local_addr().unwrap(), lcd.local_addr().unwrap()];

        // Stand-ins for the two slots' video egress ports.
        let slot1_port = free_port().await;
        let slot2_port = free_port().await;
        let port_for = move |slot: u8| match slot {
            1 => slot1_port,
            2 => slot2_port,
            other => panic!("no test port for slot {other}"),
        };

        let counters = FanoutCounters::new();
        let fanout = {
            let counters = counters.clone();
            tokio::spawn(async move {
                let _ = run_repointing_fanout(
                    || resolve_fanout_slot(1, &hero_path, &registry_path),
                    port_for,
                    &targets,
                    &counters,
                    Duration::from_millis(25),
                )
                .await;
            })
        };

        // Both drones are transmitting; only the hero's stream may come through.
        let read_one = async {
            let mut buf = [0u8; 64];
            let n = consumer.recv(&mut buf).await.unwrap();
            buf[..n].to_vec()
        };
        let got = tokio::select! {
            _ = drive(slot1_port, b"slot-1-stream") => unreachable!("the driver never returns"),
            _ = drive(slot2_port, b"slot-2-stream") => unreachable!("the driver never returns"),
            res = tokio::time::timeout(Duration::from_secs(5), read_one) => {
                res.expect("nothing reached the downstream consumer")
            }
        };
        assert_eq!(
            got, b"slot-2-stream",
            "the fan-out forwarded the wrong drone: the hero is on slot 2"
        );

        fanout.abort();
    }

    #[tokio::test]
    async fn the_fan_out_follows_the_hero_when_the_selection_changes() {
        // A hero change must reach the fan-out, or the first selection an
        // operator makes on a fleet that auto-promoted its lowest slot would
        // never take effect.
        let dir = tempfile::tempdir().unwrap();
        let registry_path = registry_with(dir.path(), &["drone-a", "drone-b"]);
        let hero_path = dir.path().join("fleet-hero.json");

        let consumer = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let targets = [consumer.local_addr().unwrap()];

        let slot1_port = free_port().await;
        let slot2_port = free_port().await;
        let port_for = move |slot: u8| match slot {
            1 => slot1_port,
            2 => slot2_port,
            other => panic!("no test port for slot {other}"),
        };

        let watch_path = hero_path.clone();
        let fanout = tokio::spawn(async move {
            let _ = run_repointing_fanout(
                || resolve_fanout_slot(1, &watch_path, &registry_path),
                port_for,
                &targets,
                &FanoutCounters::new(),
                Duration::from_millis(25),
            )
            .await;
        });

        let exercise = async {
            let mut buf = [0u8; 64];
            // No selection published: the fallback (lowest registered slot).
            loop {
                let n = consumer.recv(&mut buf).await.unwrap();
                if &buf[..n] == b"slot-1-stream" {
                    break;
                }
            }
            // Now select the drone on slot 2, exactly as the hero route does.
            crate::fleet_hero::write_hero_to(&hero_path, 2, "drone-b").unwrap();
            // The fan-out must re-point; the slot-1 driver is still running, so
            // this only passes if it actually rebound.
            loop {
                let n = consumer.recv(&mut buf).await.unwrap();
                if &buf[..n] == b"slot-2-stream" {
                    break;
                }
            }
        };

        tokio::select! {
            _ = drive(slot1_port, b"slot-1-stream") => unreachable!("the driver never returns"),
            _ = drive(slot2_port, b"slot-2-stream") => unreachable!("the driver never returns"),
            res = tokio::time::timeout(Duration::from_secs(5), exercise) => {
                res.expect("the fan-out never followed the new hero");
            }
        }

        fanout.abort();
    }

    #[test]
    fn no_published_selection_serves_the_generation_primary() {
        // The boot state, and the permanent state of a single-drone fleet that
        // is never asked to choose. Falling back to the primary is what keeps
        // the shipped one-drone product streaming.
        let dir = tempfile::tempdir().unwrap();
        let registry_path = registry_with(dir.path(), &["drone-a", "drone-b"]);
        let hero_path = dir.path().join("fleet-hero.json");
        assert_eq!(resolve_fanout_slot(1, &hero_path, &registry_path), 1);

        // A malformed file is not a licence to point the fan-out somewhere
        // arbitrary; it reads as no selection.
        std::fs::write(&hero_path, b"{ truncated").unwrap();
        assert_eq!(resolve_fanout_slot(1, &hero_path, &registry_path), 1);
    }

    #[test]
    fn a_selection_whose_drone_has_left_the_fleet_falls_back() {
        // Slot numbers are reissued. A selection left behind by a drone that has
        // unpaired must not keep the fan-out listening on a port nothing
        // transmits to, nor follow whichever drone inherited the slot.
        let dir = tempfile::tempdir().unwrap();
        let registry_path = registry_with(dir.path(), &["drone-a", "drone-b"]);
        let hero_path = dir.path().join("fleet-hero.json");

        // Registered, on the slot it claims: honoured.
        crate::fleet_hero::write_hero_to(&hero_path, 2, "drone-b").unwrap();
        assert_eq!(resolve_fanout_slot(1, &hero_path, &registry_path), 2);

        // Gone from the fleet entirely.
        crate::fleet_hero::write_hero_to(&hero_path, 2, "drone-departed").unwrap();
        assert_eq!(resolve_fanout_slot(1, &hero_path, &registry_path), 1);

        // Registered, but on a different slot than the one published.
        crate::fleet_hero::write_hero_to(&hero_path, 1, "drone-b").unwrap();
        assert_eq!(resolve_fanout_slot(1, &hero_path, &registry_path), 1);
    }

    #[test]
    fn the_rebind_backoff_is_bounded_and_non_zero() {
        // A port that cannot be bound must not spin the reactor, and one that is
        // momentarily unbindable must come back quickly.
        assert!(!REBIND_BACKOFF.is_zero());
        assert!(REBIND_BACKOFF <= Duration::from_secs(2));
        assert!(!HERO_POLL_INTERVAL.is_zero());
    }

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
        let counters = FanoutCounters::new();
        let counters_task = counters.clone();
        let fanout = tokio::spawn(async move {
            // Ignore the never-Ok result; the task is aborted at test end.
            let _ = run_fanout(listen_addr, &targets, &counters_task).await;
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

        // The shared counter saw the forwarded datagrams (the cross-process
        // signal the sidecar surfaces). At least the ones both consumers read.
        assert!(
            counters.forwarded() >= 1,
            "fan-out forwarded counter did not advance"
        );

        fanout.abort();
    }

    #[tokio::test]
    async fn empty_targets_is_rejected() {
        let listen: SocketAddr = (std::net::Ipv4Addr::LOCALHOST, 0).into();
        let err = run_fanout(listen, &[], &FanoutCounters::new())
            .await
            .unwrap_err();
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
            let _ = run_fanout(listen_addr, &targets, &FanoutCounters::new()).await;
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
