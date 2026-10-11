//! Consume the drone's aux UPLINK (radio_id 3) and inject decoded MAVLink
//! into the local flight controller.
//!
//! The mirror of `ados-groundlink::aux_consumer` (which does the equivalent
//! job on the ground, for the opposite direction), and the receiving
//! counterpart to [`crate::aux_tee`] (which reads the FC and radiates
//! outward on radio_id 2). The drone's `wfb_rx -p3` has always run and
//! re-emitted decoded uplink datagrams to a loopback port; until now nothing
//! read that port, so a ground station's `PARAM_REQUEST_LIST` — or any other
//! client command relayed over the radio — arrived at the ground station and
//! went no further.
//!
//! Datagrams may carry several batched frames, the same way the drone's own
//! downlink tee batches: [`crate::proxies`]'s `send_client_bytes` fallback
//! (`ados-mavlink-router::aux_uplink`, the ground-side sender this consumes)
//! batches a client's outbound bytes for the same packet-rate reason, so
//! frames are split on the header-derived boundary before injection.
//!
//! ## Decode failures are not all alike
//!
//! The lane is a shared, general-purpose application channel, so a datagram
//! whose magic does not match is simply another application's traffic and is
//! counted on its own rather than read as a fault. A datagram that matched
//! our magic and version and then failed some other check is our frame
//! arriving damaged, which is worth telling apart.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ados_protocol::aux_egress::AuxEgress;
use ados_protocol::aux_mux::{self, AuxChannel, AuxDecodeError};
use ados_protocol::shutdown::Shutdown;
use serde::Serialize;
use tokio::net::UdpSocket;

use crate::aux_rpc_dedupe::RequestDedupe;
use crate::connection::{ClientOrigin, FcConnection};

/// Largest datagram read in one go. Matches `ados_protocol::aux_mux::AUX_MAX_PAYLOAD`
/// plus header room; a datagram over this size cannot be one of ours anyway.
const BUF_SIZE: usize = 4096;

/// A wedged or flow-controlled FC must not stall the recv loop that also feeds
/// the relay-proxy Request lane. Ordering matters for MAVLink so this is a
/// bound, not a spawn: a write that exceeds it is dropped and counted.
const FC_WRITE_TIMEOUT: Duration = Duration::from_millis(500);

/// How many relay requests may be in flight at once.
///
/// Each accepted request spawns a task that opens a local HTTP call and
/// buffers its response, so an unbounded spawn let the uplink's datagram rate
/// set the memory ceiling on a board with a few hundred megabytes. The send
/// side is already serialised by the response slot, so a bound here costs
/// nothing in throughput — it only stops the queue forming in heap instead of
/// in the semaphore.
///
/// A request that cannot get a permit is dropped rather than queued: the
/// ground retransmits, and the dedupe cache answers the retry from what the
/// first attempt computed.
const MAX_INFLIGHT_REQUESTS: usize = 8;

/// Receive-buffer size asked of the kernel for the uplink socket.
const AUX_RECV_BUFFER_BYTES: usize = 4 * 1024 * 1024;

/// In-flight permits for relay requests. Process-wide, like the response slot.
static REQUEST_SLOTS: std::sync::LazyLock<Arc<tokio::sync::Semaphore>> =
    std::sync::LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(MAX_INFLIGHT_REQUESTS)));

#[derive(Default)]
struct CountersInner {
    datagrams_received: AtomicU64,
    mavlink_frames: AtomicU64,
    /// MAVLink frames the FC write path confirmed it delivered in full — to the
    /// local flight controller, or onto the aux uplink when this node is
    /// relaying one. Confirmed delivery, not a future that merely returned: a
    /// frame the PIC arbiter refused, or one dropped for want of both a writer
    /// and an uplink, is not an injection and must not read as one.
    mavlink_injected: AtomicU64,
    /// MAVLink frames that did not reach the flight controller: the write did
    /// not finish inside [`FC_WRITE_TIMEOUT`], the write failed, the PIC arbiter
    /// refused an injector's frame, or there was neither a local writer nor an
    /// aux uplink to take it. Non-zero with a healthy uplink means the FC link
    /// is wedged or flow-controlled; bounding the write is what keeps a
    /// relay-proxy Request behind it from being dropped by the kernel.
    mavlink_write_timeouts: AtomicU64,
    /// FC-bound frames addressed to another aircraft's system id and dropped.
    ///
    /// The ground station transmits the uplink once for the whole fleet, so
    /// every drone hears every operator command. A frame whose `target_system`
    /// is neither broadcast (0) nor this drone's own flight controller is for
    /// a different aircraft; handing it to this FC is how one ARM became an
    /// ARM on every drone that shared the system id.
    mavlink_foreign_target_dropped: AtomicU64,
    /// FC-bound frames with a non-broadcast `target_system` dropped because
    /// this drone has not yet latched its flight controller's system id from a
    /// HEARTBEAT. Without that id the frame cannot be proven to be ours.
    mavlink_target_unknown_dropped: AtomicU64,
    decode_foreign: AtomicU64,
    decode_damaged: AtomicU64,
    non_mavlink_channel: AtomicU64,
    /// Relay-proxy Request frames received. Sub-tally of `datagrams_received`.
    rpc_requests: AtomicU64,
    /// Relay-proxy Request frames addressed to a different device id, dropped
    /// without an answer so the requesting ground station's call falls to its
    /// own timeout rather than getting this drone's answer for another drone.
    rpc_requests_not_for_us: AtomicU64,
    /// Relay-proxy Request frames whose body did not decode as an RPC request.
    rpc_undecodable: AtomicU64,
    /// Relayed requests refused because their per-pair credential did not
    /// verify. Counted separately from every other refusal so an operator can
    /// tell a credential problem from a transport one: a lane that is dropping
    /// frames and a ground station presenting the wrong secret look identical
    /// from the far end otherwise.
    rpc_requests_unauthorized: AtomicU64,
    /// Ground-measured link-quality reports received. A drone cannot measure
    /// its own downlink, so this counter is how an operator tells "the ladder
    /// has no sample because the ground is not reporting" apart from "the
    /// ladder has a sample and chose to hold".
    link_feedback_frames: AtomicU64,
    /// Reports that arrived but did not decode. A lane delivering damage is a
    /// different fault from a silent one; kept separate so neither hides the
    /// other.
    link_feedback_undecodable: AtomicU64,
    /// Reports that decoded but could not be published for the ladder to read.
    link_feedback_write_errors: AtomicU64,
    /// Requests shed because the in-flight ceiling was already reached. The
    /// ground retransmits and the dedupe cache answers the retry, so this is a
    /// capacity signal rather than a lost call.
    rpc_requests_shed: AtomicU64,
    /// Reports that measured a DIFFERENT slot. Normal and expected on every
    /// drone but the one being measured — the ground transmits the uplink once
    /// for the whole fleet, so each drone sees every record and keeps only its
    /// own. A drone whose accepted count stays 0 while this climbs is simply
    /// not the slot the ground station is measuring.
    link_feedback_not_for_us: AtomicU64,
    /// Responses abandoned because the process-wide send slot did not free up
    /// inside the ground's call bound. Non-zero means concurrent relay traffic
    /// is queueing deeper than the uplink can drain, which is a capacity
    /// signal rather than a fault — the ground retransmits and the dedupe
    /// cache replays the already-computed answer.
    rpc_response_abandoned: AtomicU64,
    /// Relay-proxy Request frames whose id was already in flight. The ground
    /// retransmits an unanswered Request, so a duplicate that arrives while the
    /// original is still running is dropped — the original answers both.
    rpc_requests_duplicate: AtomicU64,
    /// Relay-proxy Request frames answered from the dedupe cache instead of by
    /// re-running the HTTP call. This is what makes the ground's retransmission
    /// safe: a retried write replays the first answer rather than writing twice.
    rpc_requests_replayed: AtomicU64,
    /// Config-over-radio request frames received: a config request the ground
    /// station's injector sent, handed to the local config-tunnel service, which
    /// serves it against `/api/config` and answers over the aux downlink.
    config_tunnel_frames: AtomicU64,
    /// Config-tunnel frames this hop handed to the local ingress socket without
    /// error. Sub-tally of `config_tunnel_frames`, and the pair to
    /// `config_tunnel_undelivered`.
    ///
    /// It does NOT mean the tunnel service read them. The ingress is an
    /// unconnected UDP socket by design, so an absent listener cannot latch an
    /// error onto it; the cost is that a send to an unbound port still
    /// succeeds. On a node with the channel opted out this counter advances and
    /// `config_tunnel_undelivered` stays at zero.
    config_tunnel_forwarded: AtomicU64,
    /// Config-tunnel frames the hand-off itself failed on: the ingress socket
    /// could not be bound, or the send returned an error. Sub-tally of
    /// `config_tunnel_frames`. Not the same thing as the service being absent,
    /// which this cannot see.
    config_tunnel_undelivered: AtomicU64,
    /// Application datagrams received on the two reserved application channels
    /// (`AppStream` 8 / `AppCommand` 9): ground → drone traffic addressed to an
    /// on-board plugin, not to the flight controller. Republished to the radio
    /// service's app fan-out so a plugin subscribed to `radio-aux.sock` receives
    /// it; these must NEVER go down the FC serial cable.
    app_datagrams: AtomicU64,
    /// Application datagrams the republish refused or could not deliver: a
    /// missing or wedged radio command socket, or the operator's aux dead-switch
    /// off. Sub-tally of `app_datagrams`.
    ///
    /// A republish that reached ZERO subscribers is NOT counted here. That is
    /// the normal state of a node with no plugin subscribed, and counting it
    /// would make a healthy lane report total loss.
    app_datagrams_undelivered: AtomicU64,
}

#[derive(Clone, Default)]
pub struct AuxUplinkConsumerCounters(Arc<CountersInner>);

#[derive(Debug, Default, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct AuxUplinkConsumerSnapshot {
    pub datagrams_received: u64,
    pub mavlink_frames: u64,
    pub mavlink_injected: u64,
    pub mavlink_write_timeouts: u64,
    pub mavlink_foreign_target_dropped: u64,
    pub mavlink_target_unknown_dropped: u64,
    pub decode_foreign: u64,
    pub decode_damaged: u64,
    pub non_mavlink_channel: u64,
    pub rpc_requests: u64,
    pub rpc_requests_not_for_us: u64,
    pub rpc_undecodable: u64,
    pub rpc_requests_unauthorized: u64,
    pub link_feedback_frames: u64,
    pub link_feedback_undecodable: u64,
    pub link_feedback_write_errors: u64,
    pub link_feedback_not_for_us: u64,
    pub rpc_requests_shed: u64,
    pub rpc_response_abandoned: u64,
    pub rpc_requests_duplicate: u64,
    pub rpc_requests_replayed: u64,
    pub config_tunnel_frames: u64,
    pub config_tunnel_forwarded: u64,
    pub config_tunnel_undelivered: u64,
    pub app_datagrams: u64,
    pub app_datagrams_undelivered: u64,
}

impl AuxUplinkConsumerCounters {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> AuxUplinkConsumerSnapshot {
        let c = &self.0;
        AuxUplinkConsumerSnapshot {
            datagrams_received: c.datagrams_received.load(Ordering::Relaxed),
            mavlink_frames: c.mavlink_frames.load(Ordering::Relaxed),
            mavlink_injected: c.mavlink_injected.load(Ordering::Relaxed),
            mavlink_write_timeouts: c.mavlink_write_timeouts.load(Ordering::Relaxed),
            mavlink_foreign_target_dropped: c
                .mavlink_foreign_target_dropped
                .load(Ordering::Relaxed),
            mavlink_target_unknown_dropped: c
                .mavlink_target_unknown_dropped
                .load(Ordering::Relaxed),
            decode_foreign: c.decode_foreign.load(Ordering::Relaxed),
            decode_damaged: c.decode_damaged.load(Ordering::Relaxed),
            non_mavlink_channel: c.non_mavlink_channel.load(Ordering::Relaxed),
            rpc_requests: c.rpc_requests.load(Ordering::Relaxed),
            rpc_requests_not_for_us: c.rpc_requests_not_for_us.load(Ordering::Relaxed),
            rpc_undecodable: c.rpc_undecodable.load(Ordering::Relaxed),
            rpc_requests_unauthorized: c.rpc_requests_unauthorized.load(Ordering::Relaxed),
            link_feedback_frames: c.link_feedback_frames.load(Ordering::Relaxed),
            link_feedback_undecodable: c.link_feedback_undecodable.load(Ordering::Relaxed),
            link_feedback_write_errors: c.link_feedback_write_errors.load(Ordering::Relaxed),
            link_feedback_not_for_us: c.link_feedback_not_for_us.load(Ordering::Relaxed),
            rpc_requests_shed: c.rpc_requests_shed.load(Ordering::Relaxed),
            rpc_response_abandoned: c.rpc_response_abandoned.load(Ordering::Relaxed),
            rpc_requests_duplicate: c.rpc_requests_duplicate.load(Ordering::Relaxed),
            rpc_requests_replayed: c.rpc_requests_replayed.load(Ordering::Relaxed),
            config_tunnel_frames: c.config_tunnel_frames.load(Ordering::Relaxed),
            config_tunnel_forwarded: c.config_tunnel_forwarded.load(Ordering::Relaxed),
            config_tunnel_undelivered: c.config_tunnel_undelivered.load(Ordering::Relaxed),
            app_datagrams: c.app_datagrams.load(Ordering::Relaxed),
            app_datagrams_undelivered: c.app_datagrams_undelivered.load(Ordering::Relaxed),
        }
    }

    /// A retransmitted Request dropped because its original is still running.
    ///
    /// Bumped from [`crate::aux_rpc_handler`], which owns the dedupe verdict and
    /// is therefore the only place that can tell these two outcomes apart; the
    /// tally lives here so it rides the one uplink snapshot with everything else
    /// about the lane.
    /// A relayed request refused because its credential did not verify.
    pub fn note_rpc_unauthorized(&self) {
        self.0
            .rpc_requests_unauthorized
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn note_rpc_duplicate(&self) {
        self.0
            .rpc_requests_duplicate
            .fetch_add(1, Ordering::Relaxed);
    }

    /// A response abandoned because the send slot never freed up in time.
    pub fn note_rpc_response_abandoned(&self) {
        self.0
            .rpc_response_abandoned
            .fetch_add(1, Ordering::Relaxed);
    }

    /// A retransmitted Request answered from the cache rather than re-executed.
    pub fn note_rpc_replayed(&self) {
        self.0.rpc_requests_replayed.fetch_add(1, Ordering::Relaxed);
    }
}

/// Bind `port` (the drone's aux-uplink re-emit loopback — `WfbConfig::aux_rx_port`,
/// default 5603) and inject every decoded MAVLink frame into `fc`. Relay-proxy
/// Request frames addressed to `own_device_id` (or broadcast) are forwarded to
/// the local HTTP API via [`crate::aux_rpc_handler`], with `dedupe` making the
/// ground's retransmissions idempotent. Config-over-radio frames are handed to
/// the local config-tunnel service through `config_tunnel`, which is inert
/// unless an operator opted that channel in. Runs until `cancel` fires; a bind
/// failure is logged and the task exits (the uplink is simply unavailable,
/// matching how a missing aux receiver already degrades elsewhere in this
/// pipeline).
pub async fn run(
    port: u16,
    deps: UplinkDeps,
    counters: AuxUplinkConsumerCounters,
    cancel: Shutdown,
) {
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let sock = match UdpSocket::bind(addr).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, port, "aux_uplink_consumer_bind_failed");
            return;
        }
    };
    set_recv_buffer(&sock);
    tracing::info!(port, "aux_uplink_consumer_started");

    let mut buf = [0u8; BUF_SIZE];
    loop {
        tokio::select! {
            biased;
            _ = cancel.wait() => break,
            recvd = sock.recv(&mut buf) => {
                match recvd {
                    Ok(n) => {
                        counters.0.datagrams_received.fetch_add(1, Ordering::Relaxed);
                        dispatch(&buf[..n], &deps, &counters).await;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "aux_uplink_consumer_recv_failed");
                    }
                }
            }
        }
    }
    tracing::info!(port, "aux_uplink_consumer_stopped");
}

/// Ask the kernel for [`AUX_RECV_BUFFER_BYTES`] of receive buffer on the
/// uplink socket.
///
/// Relay-proxy Request datagrams share this socket with batched MAVLink, and a
/// burst that lands while the loop is mid-dispatch survives only if the kernel
/// holds it. Until now the only reason the buffer was large was an unrelated
/// best-effort `net.core.rmem_default` the installer raises for video, which is
/// not a contract this lane can rely on. The kernel clamps the request to
/// `rmem_max` and reports back double the value it kept (its own bookkeeping
/// overhead), so what was actually obtained is logged rather than assumed.
/// Failure is non-fatal: the default buffer still works, it just tolerates a
/// smaller burst.
fn set_recv_buffer(sock: &UdpSocket) {
    let opts = socket2::SockRef::from(sock);
    if let Err(e) = opts.set_recv_buffer_size(AUX_RECV_BUFFER_BYTES) {
        tracing::warn!(
            error = %e,
            requested = AUX_RECV_BUFFER_BYTES,
            "aux_uplink_consumer_rcvbuf_set_failed"
        );
        return;
    }
    match opts.recv_buffer_size() {
        Ok(actual) => tracing::info!(
            requested = AUX_RECV_BUFFER_BYTES,
            actual,
            "aux_uplink_consumer_rcvbuf"
        ),
        Err(e) => tracing::warn!(error = %e, "aux_uplink_consumer_rcvbuf_read_failed"),
    }
}

/// Latches the first target/own-id mismatch, so a busy shared link cannot
/// flood the journal with a line that only matters the first time.
static TARGET_MISMATCH_LOGGED: AtomicBool = AtomicBool::new(false);

/// Report the first request this drone dropped as another node's.
///
/// A drop for a genuinely different drone is routine on a shared link, but this
/// code path is also the only visible symptom of the failure that is not: a
/// ground station that knows this drone by one id while `/etc/ados/device-id`
/// holds another (an 8-char pairing id against a 12-char device id, say) has
/// every one of its calls silently discarded. Carrying both values turns a mute
/// lane into a one-line diagnosis.
fn warn_target_mismatch_once(target: &[u8], own_device_id: &str) {
    if TARGET_MISMATCH_LOGGED.swap(true, Ordering::Relaxed) {
        return;
    }
    tracing::warn!(
        target_id = %String::from_utf8_lossy(target),
        own_id = %own_device_id,
        "aux_rpc_target_mismatch"
    );
}

/// Everything a decoded uplink frame may need, by channel.
///
/// Grouped rather than passed one by one: the read loop and the dispatcher both
/// carry the whole set, so each added channel widened two signatures at once and
/// pushed them past the argument limit. Adding a channel is now one field.
pub struct UplinkDeps {
    pub fc: Arc<FcConnection>,
    pub egress: Option<Arc<AuxEgress>>,
    pub own_device_id: Arc<str>,
    pub dedupe: Arc<RequestDedupe>,
    pub config_tunnel: Option<Arc<ados_protocol::config_tunnel_ingest::ConfigTunnelIngest>>,
}

mod dispatch;
use dispatch::*;

#[cfg(test)]
mod tests;
