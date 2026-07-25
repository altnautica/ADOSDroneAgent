//! Consume the decoded auxiliary application lane and republish its MAVLink.
//!
//! The receive chain runs a fourth decoder for the radio's general-purpose
//! application lane, which re-emits whole decoded datagrams to a loopback port.
//! Until now nothing read that port: the decoder ran, the drone transmitted, and
//! every datagram landed in a socket with no reader. This module is the reader.
//!
//! Each datagram is decoded by the shared aux framing, which is the only place
//! either rig decides what an aux datagram means. MAVLink frames are handed to
//! this node's own MAVLink republish seam, so the transports the MAVLink router
//! already serves carry the drone's vehicle and a ground control station
//! connected to this ground station sees it exactly as it would on a direct
//! link. The other channels are counted and ignored until something wants them.
//!
//! ## The lane is shared, so decode failures are not all alike
//!
//! A datagram whose magic does not match is another application's traffic on a
//! shared lane. That is normal and expected, and it is counted on its own so it
//! never reads as a fault. A datagram that matched our magic and version and
//! then failed its own length check is OUR frame arriving damaged, which is a
//! real transport fault worth alarming on. Collapsing the two into one "decode
//! errors" number would bury the second under the first, so every failure mode
//! is counted separately.
//!
//! ## Bounded, and unable to disturb the video lane
//!
//! Nothing here queues. A datagram is decoded and republished or dropped, in
//! that order, before the next is read. The republish seam is bounded by its own
//! write timeout, so a stalled MAVLink router costs one timeout rather than
//! parking this loop forever; datagrams arriving during that window are absorbed
//! by the kernel receive buffer and then dropped by the kernel, which is a
//! bounded loss rather than unbounded growth. A seam that will not accept a
//! connection is retried on a backoff instead of once per frame, so a router
//! that is down cannot turn telemetry rates into a connection storm. The video
//! lane is a separate decoder feeding a separate fan-out in a separate process
//! and shares none of this.
//!
//! ## What a republish count proves
//!
//! `mavlink_republished` counts frames the republish seam accepted. It does not
//! prove a ground control station was connected, or rendered anything. The
//! router's own published counter is the next link in that chain, and a
//! connected client is the one after it.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ados_protocol::aux_mux::{self, AuxChannel, AuxDecodeError};
use ados_protocol::mavlink_ingest::{IngestError, MavlinkIngest};
use ados_protocol::node_status::{NodeIdentity, NodeStatus};

use crate::aux_peers::AuxPeerCache;
use serde::Serialize;
use tokio::net::UdpSocket;
use tokio::sync::Notify;

/// Largest datagram read in one go.
///
/// An aux frame is capped well below this by the framing itself; the headroom
/// means an oversized datagram is read and rejected by the decoder rather than
/// silently truncated by the read and then misparsed as a damaged frame of ours.
const BUF_SIZE: usize = 4096;

/// First retry delay after the republish seam refuses a connection.
const SEAM_BACKOFF_MIN: Duration = Duration::from_secs(1);

/// Ceiling on the republish retry delay, so a router that is down for a long
/// time is probed occasionally rather than continuously.
const SEAM_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// First retry delay after a failed bind of the lane's loopback port.
const BIND_BACKOFF_MIN: Duration = Duration::from_secs(1);

/// Ceiling on the bind retry delay.
const BIND_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// How often the counters are logged, and only when something changed.
const COUNTER_REPORT_INTERVAL: Duration = Duration::from_secs(60);

/// Cumulative counters for the auxiliary lane's consumer, cloneable across the
/// tasks that read them.
///
/// Every datagram that arrives is accounted for by exactly one of the received
/// paths, so the tally describes the whole lane rather than only its successes.
/// The stats reader folds a snapshot onto the ground receive sidecar, which is
/// how a lane that is receiving nothing can be told apart from one that is
/// receiving damage, and both from one that is working.
#[derive(Clone, Default)]
pub struct AuxCounters(Arc<AuxCountersInner>);

#[derive(Default)]
struct AuxCountersInner {
    datagrams_received: AtomicU64,
    bytes_received: AtomicU64,
    mavlink_frames: AtomicU64,
    status_frames: AtomicU64,
    identity_frames: AtomicU64,
    mavlink_republished: AtomicU64,
    republish_errors: AtomicU64,
    republish_lane_down: AtomicU64,
    decode_foreign: AtomicU64,
    decode_runt: AtomicU64,
    decode_unsupported_version: AtomicU64,
    decode_unknown_channel: AtomicU64,
    decode_damaged: AtomicU64,
    decode_overlong: AtomicU64,
    status_undecodable: AtomicU64,
    status_out_of_order: AtomicU64,
    identity_undecodable: AtomicU64,
    recv_errors: AtomicU64,
    bind_failures: AtomicU64,
}

impl AuxCounters {
    /// A fresh zeroed counter set.
    pub fn new() -> Self {
        Self::default()
    }

    fn bump(&self, field: &AtomicU64) {
        field.fetch_add(1, Ordering::Relaxed);
    }

    /// A point-in-time copy for the sidecar and the periodic log line.
    pub fn snapshot(&self) -> AuxCountersSnapshot {
        let c = &self.0;
        AuxCountersSnapshot {
            datagrams_received: c.datagrams_received.load(Ordering::Relaxed),
            bytes_received: c.bytes_received.load(Ordering::Relaxed),
            mavlink_frames: c.mavlink_frames.load(Ordering::Relaxed),
            status_frames: c.status_frames.load(Ordering::Relaxed),
            identity_frames: c.identity_frames.load(Ordering::Relaxed),
            mavlink_republished: c.mavlink_republished.load(Ordering::Relaxed),
            republish_errors: c.republish_errors.load(Ordering::Relaxed),
            republish_lane_down: c.republish_lane_down.load(Ordering::Relaxed),
            decode_foreign: c.decode_foreign.load(Ordering::Relaxed),
            decode_runt: c.decode_runt.load(Ordering::Relaxed),
            decode_unsupported_version: c.decode_unsupported_version.load(Ordering::Relaxed),
            decode_unknown_channel: c.decode_unknown_channel.load(Ordering::Relaxed),
            decode_damaged: c.decode_damaged.load(Ordering::Relaxed),
            decode_overlong: c.decode_overlong.load(Ordering::Relaxed),
            status_undecodable: c.status_undecodable.load(Ordering::Relaxed),
            status_out_of_order: c.status_out_of_order.load(Ordering::Relaxed),
            identity_undecodable: c.identity_undecodable.load(Ordering::Relaxed),
            recv_errors: c.recv_errors.load(Ordering::Relaxed),
            bind_failures: c.bind_failures.load(Ordering::Relaxed),
        }
    }
}

/// A plain copy of [`AuxCounters`], for the sidecar and logs.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct AuxCountersSnapshot {
    /// Datagrams read off the lane's loopback port, decodable or not.
    pub datagrams_received: u64,
    /// Bytes those datagrams carried, framing included.
    pub bytes_received: u64,
    /// Datagrams that decoded as the MAVLink channel.
    pub mavlink_frames: u64,
    /// Datagrams that decoded as the node-status channel. Counted and ignored:
    /// nothing consumes them yet, and a silent drop would hide that the drone is
    /// already sending them.
    pub status_frames: u64,
    /// Datagrams that decoded as the identity channel, counted and ignored on
    /// the same footing as the status channel.
    pub identity_frames: u64,
    /// MAVLink frames the republish seam accepted. Not proof a ground control
    /// station was connected or rendered them.
    pub mavlink_republished: u64,
    /// MAVLink frames the republish seam rejected or failed to take.
    pub republish_errors: u64,
    /// MAVLink frames dropped without an attempt because the seam was known to
    /// be down and its retry had not come due. Distinct from an error: this is
    /// the backoff working, not a fresh failure.
    pub republish_lane_down: u64,
    /// Datagrams whose magic did not match. Another application's traffic on a
    /// shared lane, which is normal and not a fault.
    pub decode_foreign: u64,
    /// Datagrams too short to hold even a header. Ambiguous by nature: a runt
    /// from elsewhere and a badly truncated frame of ours look identical at this
    /// length, so it is neither counted as foreign nor as damage.
    pub decode_runt: u64,
    /// Datagrams carrying our magic but a framing version this build does not
    /// speak. Points at a version mismatch between the two rigs.
    pub decode_unsupported_version: u64,
    /// Datagrams carrying a channel this build does not know. Points at a newer
    /// peer using a channel added since this build.
    pub decode_unknown_channel: u64,
    /// Datagrams carrying our magic and version whose declared length did not
    /// match the bytes present. This is OUR frame arriving damaged, and is the
    /// counter that means a real transport fault.
    pub decode_damaged: u64,
    /// Datagrams declaring a payload larger than one aux frame may carry. Our
    /// magic and version matched, so this is damage rather than foreign traffic.
    pub decode_overlong: u64,
    /// Status datagrams whose body did not decode as a node snapshot: a
    /// malformed payload, or a schema version this build does not speak. A
    /// SUB-TALLY of `status_frames`, not a terminal outcome, so it is
    /// deliberately absent from [`AuxCountersSnapshot::accounted`].
    pub status_undecodable: u64,
    /// Status snapshots dropped for arriving older than the one already held.
    /// Normal in small numbers on a datagram lane; a sustained climb means the
    /// lane is reordering badly. Also a sub-tally of `status_frames`.
    pub status_out_of_order: u64,
    /// Identity datagrams whose body did not decode. Sub-tally of
    /// `identity_frames`.
    pub identity_undecodable: u64,
    /// Failed reads on the loopback socket.
    pub recv_errors: u64,
    /// Failed attempts to bind the lane's loopback port. A non-zero value with
    /// nothing received means the consumer never started listening at all.
    pub bind_failures: u64,
}

impl AuxCountersSnapshot {
    /// Datagrams accounted for by exactly one received path.
    ///
    /// Every datagram that arrives leaves through one terminal outcome: a
    /// decode failure, an ignored channel, or one of the three MAVLink
    /// dispositions. This sum must therefore equal `datagrams_received`, and a
    /// test asserts it against one datagram of every kind. It exists so the
    /// claim that the tally describes the whole lane is checkable rather than
    /// merely stated: a future channel or failure mode that forgets to bump a
    /// counter shows up here as a shortfall instead of silently making the lane
    /// look quieter than it is.
    ///
    /// `mavlink_frames` is deliberately excluded, being the subtotal that
    /// `mavlink_republished + republish_errors + republish_lane_down` splits
    /// into. Counting both would double every MAVLink datagram. The status and
    /// identity decode tallies (`status_undecodable`, `status_out_of_order`,
    /// `identity_undecodable`) are excluded on the same grounds: they describe
    /// what happened to a datagram already counted by `status_frames` /
    /// `identity_frames`, so adding them would double-count it.
    pub fn accounted(&self) -> u64 {
        self.mavlink_republished
            + self.republish_errors
            + self.republish_lane_down
            + self.status_frames
            + self.identity_frames
            + self.decode_foreign
            + self.decode_runt
            + self.decode_unsupported_version
            + self.decode_unknown_channel
            + self.decode_damaged
            + self.decode_overlong
    }
}

/// Retry gate for the republish seam.
///
/// Holds the backoff so a MAVLink router that is not up is retried on a schedule
/// rather than on every single frame, which at telemetry rates would be a
/// connection storm against a process that is already struggling.
#[derive(Debug)]
struct SeamGate {
    /// When the next attempt may be made. `None` means immediately.
    next_attempt: Option<Instant>,
    backoff: Duration,
    /// Whether the current outage has been logged, so a long outage produces one
    /// line rather than one per retry.
    outage_logged: bool,
}

impl Default for SeamGate {
    fn default() -> Self {
        Self {
            next_attempt: None,
            backoff: SEAM_BACKOFF_MIN,
            outage_logged: false,
        }
    }
}

impl SeamGate {
    fn may_attempt(&self, now: Instant) -> bool {
        self.next_attempt.is_none_or(|at| now >= at)
    }

    fn on_success(&mut self) {
        if self.outage_logged {
            tracing::info!("ground_aux_republish_recovered");
        }
        self.next_attempt = None;
        self.backoff = SEAM_BACKOFF_MIN;
        self.outage_logged = false;
    }

    fn on_failure(&mut self, now: Instant, error: &IngestError) {
        if !self.outage_logged {
            self.outage_logged = true;
            tracing::warn!(error = %error, "ground_aux_republish_unavailable");
        } else {
            tracing::debug!(error = %error, "ground_aux_republish_still_unavailable");
        }
        self.next_attempt = Some(now + self.backoff);
        self.backoff = (self.backoff * 2).min(SEAM_BACKOFF_MAX);
    }
}

/// Log the counters when they have moved since the last report.
fn report(counters: &AuxCounters, last: AuxCountersSnapshot) -> AuxCountersSnapshot {
    let now = counters.snapshot();
    if now != last {
        tracing::info!(
            datagrams_received = now.datagrams_received,
            mavlink_frames = now.mavlink_frames,
            mavlink_republished = now.mavlink_republished,
            republish_errors = now.republish_errors,
            republish_lane_down = now.republish_lane_down,
            status_frames = now.status_frames,
            identity_frames = now.identity_frames,
            decode_foreign = now.decode_foreign,
            decode_damaged = now.decode_damaged,
            recv_errors = now.recv_errors,
            "ground_aux_consumer_counters"
        );
    }
    now
}

/// Decode one datagram and act on it, updating exactly one received-path
/// counter.
///
/// Split from the read loop so the whole decode-and-dispatch decision is
/// testable without a socket.
async fn dispatch(
    datagram: &[u8],
    ingest: &MavlinkIngest,
    counters: &AuxCounters,
    gate: &mut SeamGate,
    peers: &AuxPeerCache,
) {
    let c = &counters.0;
    counters.bump(&c.datagrams_received);
    c.bytes_received
        .fetch_add(datagram.len() as u64, Ordering::Relaxed);

    let (channel, payload) = match aux_mux::decode(datagram) {
        Ok(v) => v,
        Err(e) => {
            match e {
                // Not ours. Normal on a shared lane, and deliberately not
                // grouped with the damage counters below.
                AuxDecodeError::BadMagic => counters.bump(&c.decode_foreign),
                AuxDecodeError::TooShort => counters.bump(&c.decode_runt),
                AuxDecodeError::UnsupportedVersion(v) => {
                    counters.bump(&c.decode_unsupported_version);
                    tracing::debug!(version = v, "ground_aux_unsupported_framing_version");
                }
                AuxDecodeError::UnknownChannel(ch) => {
                    counters.bump(&c.decode_unknown_channel);
                    tracing::debug!(channel = ch, "ground_aux_unknown_channel");
                }
                // Past the magic and version checks, so this claimed to be our
                // frame and arrived wrong. A real transport fault.
                AuxDecodeError::LengthMismatch { declared, actual } => {
                    counters.bump(&c.decode_damaged);
                    tracing::debug!(declared, actual, "ground_aux_frame_damaged");
                }
                AuxDecodeError::TooLong(declared) => {
                    counters.bump(&c.decode_overlong);
                    tracing::debug!(declared, "ground_aux_frame_overlong");
                }
            }
            return;
        }
    };

    match channel {
        AuxChannel::Mavlink => {
            // One datagram carries several frames: the lane's loss tracks
            // packets per second rather than bytes, so the drone batches. Split
            // on the header-derived boundary, which needs no dialect knowledge
            // and so cannot desynchronise on a message id this build lacks.
            let split = ados_protocol::aux_mux::split_frames(payload);
            // A payload that yields no whole frame is forwarded intact rather
            // than dropped. A drone build that predates batching sends exactly
            // one frame per datagram, and a frame this splitter cannot measure
            // is still a frame the router may be able to read. Dropping it here
            // would lose real traffic to make the split look tidy.
            let frames: Vec<&[u8]> = if split.is_empty() {
                vec![payload]
            } else {
                split
            };
            let now = Instant::now();
            if !gate.may_attempt(now) {
                // Count every frame the datagram carried, not the datagram, so
                // sent and republished totals stay comparable.
                for _ in &frames {
                    counters.bump(&c.mavlink_frames);
                    counters.bump(&c.republish_lane_down);
                }
                return;
            }
            for frame in frames {
                counters.bump(&c.mavlink_frames);
                match ingest.send_frame(frame).await {
                    Ok(()) => {
                        gate.on_success();
                        counters.bump(&c.mavlink_republished);
                    }
                    Err(e) => {
                        gate.on_failure(now, &e);
                        counters.bump(&c.republish_errors);
                    }
                }
            }
        }
        // The relayed node describing itself. Decoded into the peer cache, whose
        // sidecar is what lets this node answer what it is relaying; a body that
        // does not decode is counted rather than dropped silently, because a
        // peer sending frames this build cannot read is a version mismatch worth
        // seeing.
        AuxChannel::Status => {
            counters.bump(&c.status_frames);
            match NodeStatus::decode(payload) {
                Some(status) => {
                    if !peers.record_status(status) {
                        counters.bump(&c.status_out_of_order);
                    }
                }
                None => counters.bump(&c.status_undecodable),
            }
        }
        AuxChannel::Identity => {
            counters.bump(&c.identity_frames);
            match NodeIdentity::decode(payload) {
                Some(identity) => peers.record_identity(identity),
                None => counters.bump(&c.identity_undecodable),
            }
        }
    }
}

/// Read the lane's loopback port until cancelled or the socket fails fatally.
///
/// Returns `Err` only when the port could not be bound; the caller supervises
/// the retry. Takes an already-built republish client so a test can point the
/// consumer at a stand-in seam.
pub async fn run_aux_consumer(
    listen_port: u16,
    ingest: Arc<MavlinkIngest>,
    counters: AuxCounters,
    peers: AuxPeerCache,
    cancel: Arc<Notify>,
) -> std::io::Result<()> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, listen_port));
    let sock = UdpSocket::bind(addr).await?;
    tracing::info!(port = listen_port, "ground_aux_consumer_listening");

    let mut gate = SeamGate::default();
    let mut buf = vec![0u8; BUF_SIZE];
    let mut last_report = AuxCountersSnapshot::default();
    let mut report_tick = tokio::time::interval_at(
        tokio::time::Instant::now() + COUNTER_REPORT_INTERVAL,
        COUNTER_REPORT_INTERVAL,
    );
    report_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Consecutive read errors with no intervening success. A datagram sent to a
    // closed peer can surface as an error on the next read; that is transient
    // and self-clears. A run of them with no successes means the socket cannot
    // be read at all, so a short sleep keeps a hard failure from spinning the
    // CPU while the caller decides what to do.
    let mut consecutive_errors: u32 = 0;

    loop {
        tokio::select! {
            biased;
            _ = cancel.notified() => break,
            _ = report_tick.tick() => {
                last_report = report(&counters, last_report);
            }
            received = sock.recv_from(&mut buf) => match received {
                Ok((n, _from)) => {
                    consecutive_errors = 0;
                    dispatch(&buf[..n], &ingest, &counters, &mut gate, &peers).await;
                }
                Err(e) => {
                    counters.bump(&counters.0.recv_errors);
                    consecutive_errors += 1;
                    tracing::debug!(error = %e, "ground_aux_recv_error");
                    if consecutive_errors >= 5 {
                        tracing::warn!(
                            error = %e,
                            consecutive = consecutive_errors,
                            "ground_aux_recv_failing"
                        );
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                }
            },
        }
    }

    report(&counters, last_report);
    tracing::info!("ground_aux_consumer_stopped");
    Ok(())
}

/// Supervise the consumer for the service lifetime, re-binding with bounded
/// backoff when the port cannot be taken.
///
/// The port is bound once for the whole service rather than per receive
/// generation: the generations respawn on link loss, and re-binding a UDP port
/// on that cadence risks losing the bind to its own lingering socket. A bind
/// failure is counted, so a consumer that never started listening is visible on
/// the sidecar rather than merely absent from it.
pub async fn supervise_aux_consumer(
    listen_port: u16,
    ingest: Arc<MavlinkIngest>,
    counters: AuxCounters,
    peers: AuxPeerCache,
    cancel: Arc<Notify>,
) {
    let mut backoff = BIND_BACKOFF_MIN;
    loop {
        match run_aux_consumer(
            listen_port,
            ingest.clone(),
            counters.clone(),
            peers.clone(),
            cancel.clone(),
        )
        .await
        {
            // A clean return means cancellation; nothing left to supervise.
            Ok(()) => return,
            Err(e) => {
                counters.bump(&counters.0.bind_failures);
                tracing::warn!(
                    port = listen_port,
                    error = %e,
                    retry_s = backoff.as_secs(),
                    "ground_aux_consumer_bind_failed"
                );
            }
        }
        tokio::select! {
            _ = cancel.notified() => return,
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(BIND_BACKOFF_MAX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::frame::HEADER_SIZE;
    use std::path::PathBuf;
    use tokio::io::AsyncReadExt;
    use tokio::net::UnixListener;
    use tokio::sync::mpsc;

    /// A stand-in for the MAVLink router's republish seam: accepts one
    /// connection and decodes the length-prefixed frames off it onto a channel,
    /// exactly as the real seam's reader does. Frames arriving here have
    /// reached the republish plane.
    fn fake_seam(path: PathBuf) -> (mpsc::Receiver<Vec<u8>>, tokio::task::JoinHandle<()>) {
        let (tx, rx) = mpsc::channel(32);
        let handle = tokio::spawn(async move {
            let listener = UnixListener::bind(&path).unwrap();
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            loop {
                let mut header = [0u8; HEADER_SIZE];
                if stream.read_exact(&mut header).await.is_err() {
                    return;
                }
                let len = u32::from_be_bytes(header) as usize;
                let mut payload = vec![0u8; len];
                if len > 0 && stream.read_exact(&mut payload).await.is_err() {
                    return;
                }
                if tx.send(payload).await.is_err() {
                    return;
                }
            }
        });
        (rx, handle)
    }

    /// A heartbeat frame, as the drone's tee would put on the lane.
    fn heartbeat() -> Vec<u8> {
        vec![
            0xFD, 0x09, 0x00, 0x00, 0x07, 0x01, 0x01, 0x00, 0x00, 0x00, 0xAA, 0xBB,
        ]
    }

    #[tokio::test]
    async fn a_mavlink_datagram_reaches_the_republish_seam_byte_for_byte() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("mavlink-ingest.sock");
        let (mut seam, server) = fake_seam(sock.clone());
        tokio::time::sleep(Duration::from_millis(20)).await;

        let ingest = MavlinkIngest::new(&sock);
        let counters = AuxCounters::new();
        let mut gate = SeamGate::default();
        let peers = AuxPeerCache::new();

        let frame = heartbeat();
        let datagram = aux_mux::encode(AuxChannel::Mavlink, &frame).unwrap();
        dispatch(&datagram, &ingest, &counters, &mut gate, &peers).await;

        assert_eq!(seam.recv().await.unwrap(), frame);
        let snap = counters.snapshot();
        assert_eq!(snap.datagrams_received, 1);
        assert_eq!(snap.mavlink_frames, 1);
        assert_eq!(snap.mavlink_republished, 1);
        assert_eq!(snap.republish_errors, 0);
        server.abort();
    }

    #[tokio::test]
    async fn a_damaged_frame_of_ours_is_counted_as_damage_not_dropped_silently() {
        // Past the magic and version checks, so this is our frame arriving
        // wrong. It must be distinguishable from foreign traffic, because one
        // is a transport fault and the other is normal.
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("mavlink-ingest.sock");
        let (mut seam, server) = fake_seam(sock.clone());
        tokio::time::sleep(Duration::from_millis(20)).await;

        let ingest = MavlinkIngest::new(&sock);
        let counters = AuxCounters::new();
        let mut gate = SeamGate::default();
        let peers = AuxPeerCache::new();

        let mut truncated = aux_mux::encode(AuxChannel::Mavlink, &heartbeat()).unwrap();
        truncated.truncate(truncated.len() - 3);
        dispatch(&truncated, &ingest, &counters, &mut gate, &peers).await;

        let snap = counters.snapshot();
        assert_eq!(snap.datagrams_received, 1);
        assert_eq!(snap.decode_damaged, 1, "damage must be counted as damage");
        assert_eq!(snap.decode_foreign, 0, "damage is not foreign traffic");
        assert_eq!(snap.mavlink_republished, 0);

        // A healthy frame after the damaged one still gets through: the lane
        // does not latch shut on one bad datagram.
        let good = aux_mux::encode(AuxChannel::Mavlink, &heartbeat()).unwrap();
        dispatch(&good, &ingest, &counters, &mut gate, &peers).await;
        assert_eq!(seam.recv().await.unwrap(), heartbeat());
        server.abort();
    }

    #[tokio::test]
    async fn a_batched_datagram_republishes_every_frame_it_carried() {
        // The drone packs several frames into one datagram because the lane
        // loses packets rather than bytes. Each must reach the seam separately
        // or the republished stream is one corrupt blob.
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("mavlink-ingest.sock");
        let (mut seam, server) = fake_seam(sock.clone());
        tokio::time::sleep(Duration::from_millis(20)).await;

        let ingest = MavlinkIngest::new(&sock);
        let counters = AuxCounters::new();
        let mut gate = SeamGate::default();
        let peers = AuxPeerCache::new();

        // A WELL-FORMED v2 frame: 10 header bytes, a 9-byte payload, 2 checksum.
        // The `heartbeat()` fixture above declares a 9-byte payload but is only
        // 12 bytes long, so it is not a whole frame and cannot be split out of a
        // batch. That is fine for the single-frame test (the fallback forwards
        // an unsplittable payload intact) but a batch needs real boundaries.
        let mut frame = vec![0xFD, 0x09, 0x00, 0x00, 0x07, 0x01, 0x01, 0x00, 0x00, 0x00];
        frame.extend_from_slice(&[0x11; 9]);
        frame.extend_from_slice(&[0xAA, 0xBB]);
        assert_eq!(frame.len(), 21, "a v2 frame with a 9-byte payload");

        let mut batch = Vec::new();
        for _ in 0..3 {
            batch.extend_from_slice(&frame);
        }
        let datagram = aux_mux::encode(AuxChannel::Mavlink, &batch).unwrap();
        dispatch(&datagram, &ingest, &counters, &mut gate, &peers).await;

        for _ in 0..3 {
            assert_eq!(seam.recv().await.unwrap(), frame);
        }
        let snap = counters.snapshot();
        assert_eq!(snap.datagrams_received, 1, "one datagram arrived");
        assert_eq!(snap.mavlink_frames, 3, "carrying three frames");
        assert_eq!(snap.mavlink_republished, 3);
        assert_eq!(snap.republish_errors, 0);
        server.abort();
    }

    #[tokio::test]
    async fn a_payload_that_is_not_splittable_is_still_forwarded() {
        // A drone build predating batching sends one frame per datagram. If the
        // splitter cannot measure it we forward it intact rather than drop it.
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("mavlink-ingest.sock");
        let (mut seam, server) = fake_seam(sock.clone());
        tokio::time::sleep(Duration::from_millis(20)).await;

        let ingest = MavlinkIngest::new(&sock);
        let counters = AuxCounters::new();
        let mut gate = SeamGate::default();
        let peers = AuxPeerCache::new();

        let opaque = b"not-a-mavlink-frame".to_vec();
        let datagram = aux_mux::encode(AuxChannel::Mavlink, &opaque).unwrap();
        dispatch(&datagram, &ingest, &counters, &mut gate, &peers).await;

        assert_eq!(seam.recv().await.unwrap(), opaque);
        assert_eq!(counters.snapshot().mavlink_republished, 1);
        server.abort();
    }

    #[tokio::test]
    async fn foreign_traffic_is_counted_apart_from_damage_and_republishes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("mavlink-ingest.sock");
        let ingest = MavlinkIngest::with_timeout(&sock, Duration::from_millis(50));
        let counters = AuxCounters::new();
        let mut gate = SeamGate::default();
        let peers = AuxPeerCache::new();

        dispatch(
            b"some other application's datagram",
            &ingest,
            &counters,
            &mut gate,
            &peers,
        )
        .await;
        dispatch(b"\xAD", &ingest, &counters, &mut gate, &peers).await;

        let snap = counters.snapshot();
        assert_eq!(snap.decode_foreign, 1);
        assert_eq!(snap.decode_runt, 1, "a runt is neither foreign nor damage");
        assert_eq!(snap.decode_damaged, 0);
        assert_eq!(snap.mavlink_frames, 0);
        assert_eq!(snap.mavlink_republished, 0);
    }

    #[tokio::test]
    async fn each_framing_failure_mode_lands_in_its_own_counter() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("mavlink-ingest.sock");
        let ingest = MavlinkIngest::with_timeout(&sock, Duration::from_millis(50));
        let counters = AuxCounters::new();
        let mut gate = SeamGate::default();
        let peers = AuxPeerCache::new();

        let mut wrong_version = aux_mux::encode(AuxChannel::Mavlink, b"x").unwrap();
        wrong_version[2] = 0x99;
        dispatch(&wrong_version, &ingest, &counters, &mut gate, &peers).await;

        let mut unknown_channel = aux_mux::encode(AuxChannel::Mavlink, b"x").unwrap();
        unknown_channel[3] = 0x7F;
        dispatch(&unknown_channel, &ingest, &counters, &mut gate, &peers).await;

        let mut overlong = aux_mux::encode(AuxChannel::Mavlink, b"x").unwrap();
        overlong[4] = 0xFF;
        overlong[5] = 0xFF;
        dispatch(&overlong, &ingest, &counters, &mut gate, &peers).await;

        let snap = counters.snapshot();
        assert_eq!(snap.decode_unsupported_version, 1);
        assert_eq!(snap.decode_unknown_channel, 1);
        assert_eq!(snap.decode_overlong, 1);
        assert_eq!(snap.datagrams_received, 3);
    }

    #[tokio::test]
    async fn the_other_channels_are_counted_and_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("mavlink-ingest.sock");
        let (mut seam, server) = fake_seam(sock.clone());
        tokio::time::sleep(Duration::from_millis(20)).await;

        let ingest = MavlinkIngest::new(&sock);
        let counters = AuxCounters::new();
        let mut gate = SeamGate::default();
        let peers = AuxPeerCache::new();

        for ch in [AuxChannel::Status, AuxChannel::Identity] {
            let d = aux_mux::encode(ch, b"a compact snapshot").unwrap();
            dispatch(&d, &ingest, &counters, &mut gate, &peers).await;
        }
        // The MAVLink one after them still republishes, proving the ignore is
        // per-channel and not a lane that stopped.
        let d = aux_mux::encode(AuxChannel::Mavlink, &heartbeat()).unwrap();
        dispatch(&d, &ingest, &counters, &mut gate, &peers).await;

        assert_eq!(seam.recv().await.unwrap(), heartbeat());
        let snap = counters.snapshot();
        assert_eq!(snap.status_frames, 1);
        assert_eq!(snap.identity_frames, 1);
        assert_eq!(snap.mavlink_republished, 1);
        server.abort();
    }

    #[tokio::test]
    async fn an_absent_seam_backs_off_instead_of_retrying_every_frame() {
        // With no router listening, the first frame attempts and fails; the
        // frames behind it are dropped against the backoff rather than each
        // paying a fresh connection attempt.
        let ingest = Arc::new(MavlinkIngest::with_timeout(
            "/nonexistent/mavlink-ingest.sock",
            Duration::from_millis(20),
        ));
        let counters = AuxCounters::new();
        let mut gate = SeamGate::default();
        let peers = AuxPeerCache::new();

        let d = aux_mux::encode(AuxChannel::Mavlink, &heartbeat()).unwrap();
        for _ in 0..5 {
            dispatch(&d, &ingest, &counters, &mut gate, &peers).await;
        }

        let snap = counters.snapshot();
        assert_eq!(snap.mavlink_frames, 5, "every frame is still accounted for");
        assert_eq!(snap.republish_errors, 1, "only the first attempt is made");
        assert_eq!(
            snap.republish_lane_down, 4,
            "the rest drop against the gate"
        );
        assert_eq!(snap.mavlink_republished, 0);
    }

    #[tokio::test]
    async fn every_datagram_lands_in_exactly_one_bucket() {
        // The lane's tally is only trustworthy if it accounts for the whole
        // stream. Feed one datagram of every kind the consumer can meet and
        // assert the outcome counters sum back to the number that arrived, so a
        // path that forgot to count would show as a shortfall rather than
        // quietly making the lane look emptier than it was.
        let ingest = MavlinkIngest::with_timeout(
            "/nonexistent/mavlink-ingest.sock",
            Duration::from_millis(20),
        );
        let counters = AuxCounters::new();
        let mut gate = SeamGate::default();
        let peers = AuxPeerCache::new();

        let mut wrong_version = aux_mux::encode(AuxChannel::Mavlink, b"x").unwrap();
        wrong_version[2] = 0x99;
        let mut unknown_channel = aux_mux::encode(AuxChannel::Mavlink, b"x").unwrap();
        unknown_channel[3] = 0x7F;
        let mut overlong = aux_mux::encode(AuxChannel::Mavlink, b"x").unwrap();
        overlong[4] = 0xFF;
        overlong[5] = 0xFF;
        let mut damaged = aux_mux::encode(AuxChannel::Mavlink, &heartbeat()).unwrap();
        damaged.truncate(damaged.len() - 3);

        let datagrams: Vec<Vec<u8>> = vec![
            aux_mux::encode(AuxChannel::Mavlink, &heartbeat()).unwrap(),
            // A second MAVLink frame: the first fails the absent seam, this one
            // drops against the backoff, so both republish outcomes are covered.
            aux_mux::encode(AuxChannel::Mavlink, &heartbeat()).unwrap(),
            aux_mux::encode(AuxChannel::Status, b"status").unwrap(),
            aux_mux::encode(AuxChannel::Identity, b"identity").unwrap(),
            b"foreign application traffic".to_vec(),
            b"\xAD".to_vec(),
            wrong_version,
            unknown_channel,
            overlong,
            damaged,
        ];
        let sent = datagrams.len() as u64;
        for d in &datagrams {
            dispatch(d, &ingest, &counters, &mut gate, &peers).await;
        }

        let snap = counters.snapshot();
        assert_eq!(snap.datagrams_received, sent);
        assert_eq!(
            snap.accounted(),
            snap.datagrams_received,
            "every datagram must be accounted for exactly once: {snap:?}"
        );
        // And the split really is populated, so the equality above is not being
        // satisfied by two empty sums.
        assert_eq!(snap.republish_errors, 1);
        assert_eq!(snap.republish_lane_down, 1);
        assert_eq!(snap.decode_damaged, 1);
        assert_eq!(snap.decode_foreign, 1);
    }

    #[tokio::test]
    async fn the_full_loop_carries_a_datagram_from_the_wire_to_the_seam() {
        // End to end over real sockets: a datagram on the lane's loopback port,
        // decoded, and the MAVLink frame arriving at the republish seam.
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("mavlink-ingest.sock");
        let (mut seam, server) = fake_seam(sock.clone());
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Port 0 lets the kernel choose, so the test never collides with a
        // running service or another test.
        let probe = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        let counters = AuxCounters::new();
        let cancel = Arc::new(Notify::new());
        let task = tokio::spawn(run_aux_consumer(
            port,
            Arc::new(MavlinkIngest::new(&sock)),
            counters.clone(),
            AuxPeerCache::new(),
            cancel.clone(),
        ));
        tokio::time::sleep(Duration::from_millis(50)).await;

        let tx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let frame = heartbeat();
        let datagram = aux_mux::encode(AuxChannel::Mavlink, &frame).unwrap();
        tx.send_to(&datagram, (Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        // A datagram that is not ours, to prove the loop keeps going past one.
        tx.send_to(b"foreign", (Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();

        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), seam.recv())
                .await
                .expect("the frame must reach the seam")
                .unwrap(),
            frame
        );

        cancel.notify_waiters();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("cancellation must stop the consumer")
            .unwrap()
            .unwrap();

        let snap = counters.snapshot();
        assert_eq!(snap.mavlink_republished, 1);
        assert!(snap.bytes_received >= datagram.len() as u64);
        server.abort();
    }

    #[tokio::test]
    async fn a_port_that_cannot_be_bound_is_reported_rather_than_silently_idle() {
        // A consumer that never started listening must be visible as such: a
        // bind failure counted, not an absent counter that reads as "nothing
        // arrived".
        let held = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = held.local_addr().unwrap().port();

        let counters = AuxCounters::new();
        let cancel = Arc::new(Notify::new());
        let ingest = Arc::new(MavlinkIngest::with_timeout(
            "/nonexistent/mavlink-ingest.sock",
            Duration::from_millis(20),
        ));

        assert!(
            run_aux_consumer(
                port,
                ingest.clone(),
                counters.clone(),
                AuxPeerCache::new(),
                cancel.clone(),
            )
            .await
            .is_err(),
            "binding a taken port must report an error"
        );

        // The supervisor turns that error into a counted, retried outage rather
        // than a task that quietly disappears. Aborted rather than cancelled
        // here: the cancellation path is asserted deterministically in the
        // full-loop test above, and racing a notification against a task that
        // is between its two await points would only make this one flaky.
        let task = tokio::spawn(supervise_aux_consumer(
            port,
            ingest,
            counters.clone(),
            AuxPeerCache::new(),
            cancel.clone(),
        ));
        tokio::time::sleep(Duration::from_millis(100)).await;
        task.abort();

        assert!(
            counters.snapshot().bind_failures >= 1,
            "the failed bind must be counted"
        );
        drop(held);
    }

    /// A status frame must actually REACH the cache, not merely be counted.
    ///
    /// The counter and the cache are updated in the same arm, so a wiring
    /// regression that dropped the decode would still leave `status_frames`
    /// climbing and the surface silently empty. This asserts the surface.
    #[tokio::test]
    async fn status_and_identity_frames_reach_the_peer_cache() {
        let ingest = MavlinkIngest::new("/nonexistent/ingest.sock");
        let counters = AuxCounters::new();
        let mut gate = SeamGate::default();
        let peers = AuxPeerCache::new();

        let (status_body, _) = ados_protocol::node_status::NodeStatus::new("drone-a", 3)
            .with_fc(Some(true), Some(true), None, Some("ardupilot"))
            .encode()
            .unwrap();
        let identity_body = ados_protocol::node_status::NodeIdentity::build(
            "drone-a",
            Some("Alpha"),
            Some("drone"),
            Some("1.2.3"),
        )
        .encode()
        .unwrap();

        for (channel, body) in [
            (AuxChannel::Status, status_body),
            (AuxChannel::Identity, identity_body),
        ] {
            let framed = aux_mux::encode(channel, &body).unwrap();
            dispatch(&framed, &ingest, &counters, &mut gate, &peers).await;
        }

        let snap = counters.snapshot();
        assert_eq!(snap.status_frames, 1);
        assert_eq!(snap.identity_frames, 1);
        assert_eq!(snap.status_undecodable, 0);
        assert_eq!(snap.identity_undecodable, 0);

        let published = peers.peers_payload(crate::aux_peers::test_now());
        assert_eq!(published.len(), 1, "the peer must be published");
        assert_eq!(published[0]["device_id"], "drone-a");
        assert_eq!(published[0]["name"], "Alpha");
        assert_eq!(published[0]["status"]["fc_firmware"], "ardupilot");
    }

    /// A status body this build cannot read is counted, not silently dropped:
    /// a peer running a newer schema is a version mismatch worth seeing.
    #[tokio::test]
    async fn an_undecodable_status_body_is_counted_separately() {
        let ingest = MavlinkIngest::new("/nonexistent/ingest.sock");
        let counters = AuxCounters::new();
        let mut gate = SeamGate::default();
        let peers = AuxPeerCache::new();

        let framed = aux_mux::encode(AuxChannel::Status, b"not msgpack").unwrap();
        dispatch(&framed, &ingest, &counters, &mut gate, &peers).await;

        let snap = counters.snapshot();
        assert_eq!(snap.status_frames, 1, "the datagram is still accounted for");
        assert_eq!(snap.status_undecodable, 1);
        assert!(peers.peers_payload(0.0).is_empty());
        // The sub-tally must not disturb the whole-lane accounting invariant.
        assert_eq!(snap.accounted(), snap.datagrams_received);
    }
}
