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
use serde::Serialize;
use tokio::net::UdpSocket;
use tokio::sync::Notify;

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
    mavlink_injected: AtomicU64,
    /// MAVLink frames dropped because the FC write did not finish inside
    /// [`FC_WRITE_TIMEOUT`]. Non-zero means the FC link is wedged or flow-
    /// controlled; the uplink itself is fine, and bounding the write is what
    /// keeps a relay-proxy Request behind it from being dropped by the kernel.
    mavlink_write_timeouts: AtomicU64,
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
}

#[derive(Clone, Default)]
pub struct AuxUplinkConsumerCounters(Arc<CountersInner>);

#[derive(Debug, Default, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct AuxUplinkConsumerSnapshot {
    pub datagrams_received: u64,
    pub mavlink_frames: u64,
    pub mavlink_injected: u64,
    pub mavlink_write_timeouts: u64,
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
    cancel: Arc<Notify>,
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
            _ = cancel.notified() => break,
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

async fn dispatch(payload: &[u8], deps: &UplinkDeps, counters: &AuxUplinkConsumerCounters) {
    let (channel, inner) = match aux_mux::decode(payload) {
        Ok(v) => v,
        Err(AuxDecodeError::BadMagic) => {
            counters.0.decode_foreign.fetch_add(1, Ordering::Relaxed);
            return;
        }
        Err(_) => {
            counters.0.decode_damaged.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };

    match channel {
        AuxChannel::Mavlink => {
            // A sender may batch several frames into one datagram (see
            // `aux_uplink::run`'s own batching); split on the header-derived
            // boundary, which needs no dialect knowledge. A payload that yields no
            // whole frame is injected intact rather than dropped — an older sender
            // that predates batching sends exactly one frame per datagram.
            let split = aux_mux::split_frames(inner);
            let frames: Vec<&[u8]> = if split.is_empty() { vec![inner] } else { split };
            counters
                .0
                .mavlink_frames
                .fetch_add(frames.len() as u64, Ordering::Relaxed);
            for frame in frames {
                // Item 13 — a private-payload TUNNEL frame (payload_type above
                // the reserved max) is plugin-addressed APPLICATION traffic (an
                // AD05-style cross-link frame), not an FC command. Publish it to
                // the router's frame fan-out so an on-board plugin subscribed to
                // TUNNEL actually receives the ground→drone frame. It must NEVER
                // go down the FC serial cable — where, before this, it was handed
                // to the FC and silently disappeared (the drone-side plugin never
                // saw the operator's command).
                if let Some(pt) = ados_protocol::mavlink::tunnel_payload_type(frame) {
                    if pt > ados_protocol::mavlink::TUNNEL_RESERVED_PAYLOAD_TYPE_MAX {
                        deps.fc.inject_frame(frame.to_vec());
                        counters.0.mavlink_injected.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                }
                // Item 14 (safety-critical) — a genuine FC-bound uplink frame is
                // routed through the ARBITRATED client path, not the ungated
                // serial write it used to take. `send_client_bytes` is the single
                // PIC choke point the drone's own autonomous injector also passes
                // through, so a relayed operator command and an autonomous write
                // are adjudicated by one decision function and cannot drift. The
                // relayed frame carries no injector claim — it is the human path,
                // and the operator beats the injector by construction; the
                // autonomous guidance write declares its injector and is gated.
                deps.fc
                    .send_client_bytes(frame, ClientOrigin::Relayed, None)
                    .await;
                counters.0.mavlink_injected.fetch_add(1, Ordering::Relaxed);
            }
        }
        AuxChannel::Request => {
            // A relay-proxy HTTP request from the ground station. Forward it to
            // the drone's own HTTP API and send the response back over the aux
            // downlink. Runs in a spawned task so the uplink consumer's read
            // loop never stalls behind a slow HTTP call.
            counters.0.rpc_requests.fetch_add(1, Ordering::Relaxed);
            match ados_protocol::aux_rpc::decode_request(inner) {
                Ok(request) => {
                    // An unresolved local id cannot adjudicate a target, so accept
                    // everything and let the operator see it in the counter —
                    // dropping every request on a drone that simply has not been
                    // provisioned yet is a far worse failure. A named target that
                    // is not us is still dropped WITHOUT an answer, so the ground
                    // never mistakes this drone's response for the one it asked for.
                    let addressed_elsewhere = !deps.own_device_id.is_empty()
                        && !request.target.is_empty()
                        && request.target != deps.own_device_id.as_bytes();
                    if addressed_elsewhere {
                        counters
                            .0
                            .rpc_requests_not_for_us
                            .fetch_add(1, Ordering::Relaxed);
                        warn_target_mismatch_once(request.target, &deps.own_device_id);
                        return;
                    }
                    let Ok(permit) = REQUEST_SLOTS.clone().try_acquire_owned() else {
                        // Already at the in-flight ceiling. Dropping is safe
                        // and cheap here: the ground retransmits an unanswered
                        // request, and the dedupe cache serves the retry from
                        // the first attempt's work.
                        counters.0.rpc_requests_shed.fetch_add(1, Ordering::Relaxed);
                        return;
                    };
                    if let Some(egress) = &deps.egress {
                        let id = request.id;
                        let method = request.method;
                        let path = request.path.to_vec();
                        let body = request.body.to_vec();
                        let ticket = request.ticket.to_vec();
                        let egress = Arc::clone(egress);
                        let dedupe = Arc::clone(&deps.dedupe);
                        let counters = counters.clone();
                        let sender = deps.own_device_id.to_string();
                        tokio::spawn(async move {
                            let _permit = permit;
                            let req = ados_protocol::aux_rpc::RpcRequest {
                                id,
                                method,
                                target: &[],
                                path: &path,
                                body: &body,
                                ticket: &ticket,
                            };
                            crate::aux_rpc_handler::handle(
                                &req, &egress, &dedupe, &counters, &sender,
                            )
                            .await;
                        });
                    } else {
                        tracing::debug!("aux_rpc_request_no_egress");
                    }
                }
                Err(_) => {
                    counters.0.rpc_undecodable.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        AuxChannel::LinkFeedback => {
            // The ground station reporting what it actually decoded of our
            // downlink. This is the only honest loss measurement a transmitting
            // drone can obtain — its own radio is in monitor mode and cannot
            // capture its own injected frames — so publish it for the radio
            // service's adaptive bitrate ladder to read.
            counters
                .0
                .link_feedback_frames
                .fetch_add(1, Ordering::Relaxed);
            match ados_protocol::link_feedback::LinkFeedback::decode(inner) {
                Ok(fb) => {
                    // The uplink is ONE transmission for the whole fleet, so
                    // every drone receives every record. Acting on one that
                    // measures a different slot would steer this drone's video
                    // rate from a neighbour's link — a false measurement, and
                    // worse than having none, because a no-sample ladder at
                    // least holds honestly.
                    //
                    // An unprovisioned node (no slot in config) accepts
                    // nothing: it cannot prove a record is about itself, and
                    // guessing is the failure this check exists to prevent.
                    let own_slot = ados_protocol::fleet_identity::local_slot();
                    if own_slot != Some(fb.target_slot) {
                        counters
                            .0
                            .link_feedback_not_for_us
                            .fetch_add(1, Ordering::Relaxed);
                        return;
                    }
                    let sidecar = ados_protocol::link_feedback::LinkFeedbackSidecar::now(&fb);
                    let path = ados_protocol::link_feedback::sidecar_path();
                    if let Err(e) = ados_protocol::link_feedback::write_sidecar_to(&path, &sidecar)
                    {
                        counters
                            .0
                            .link_feedback_write_errors
                            .fetch_add(1, Ordering::Relaxed);
                        tracing::debug!(error = %e, "link_feedback_sidecar_write_failed");
                    }
                }
                Err(_) => {
                    // Undecodable is counted separately from "not received":
                    // a lane delivering damage is a different fault from a
                    // silent one, and the ladder must not act on either.
                    counters
                        .0
                        .link_feedback_undecodable
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        // A config-over-radio frame from the ground station. The config-tunnel
        // service cannot bind this lane's port — this consumer holds it — so the
        // frame is handed to that service's own loopback ingress with its aux
        // framing INTACT, so it re-checks the channel itself instead of trusting
        // this hop. The service is the only thing that ever interprets a config
        // frame; nothing here reads or acts on the config payload.
        //
        // Forwarded, never spawned: one small datagram per chunk, so the read
        // loop pays a loopback write rather than a task.
        AuxChannel::ConfigTunnel => {
            counters
                .0
                .config_tunnel_frames
                .fetch_add(1, Ordering::Relaxed);
            match deps.config_tunnel.as_deref() {
                Some(ingest) if ingest.send(payload).await => {
                    counters
                        .0
                        .config_tunnel_forwarded
                        .fetch_add(1, Ordering::Relaxed);
                }
                // The normal case with the channel opted out: nothing is bound
                // to the ingress, so the frame is counted and dropped rather
                // than reported as a fault.
                _ => {
                    counters
                        .0
                        .config_tunnel_undelivered
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        _ => {
            counters
                .0
                .non_mavlink_channel
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MavlinkConfig;
    use crate::param_cache::ParamCache;
    use crate::state::VehicleState;
    use ados_protocol::mavlink::ardupilotmega::{
        MavAutopilot, MavMessage, MavModeFlag, MavState, MavType, HEARTBEAT_DATA,
    };
    use ados_protocol::mavlink::{self, MavHeader};
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::AsyncWrite;
    use tokio::sync::Mutex;

    /// This drone's own device id, as `/etc/ados/device-id` would carry it.
    const OWN_ID: &str = "77735cd38937";

    fn heartbeat_bytes() -> Vec<u8> {
        let msg = MavMessage::HEARTBEAT(HEARTBEAT_DATA {
            custom_mode: 0,
            mavtype: MavType::MAV_TYPE_GCS,
            autopilot: MavAutopilot::MAV_AUTOPILOT_INVALID,
            base_mode: MavModeFlag::empty(),
            system_status: MavState::MAV_STATE_ACTIVE,
            mavlink_version: 3,
        });
        mavlink::serialize_v2(
            MavHeader {
                system_id: 255,
                component_id: 190,
                sequence: 0,
            },
            &msg,
        )
        .unwrap()
    }

    /// Captures every write, standing in for a real FC serial port so a test
    /// can assert exactly what reached "the flight controller".
    struct CapturingWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl AsyncWrite for CapturingWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            data: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.0.lock().unwrap().extend_from_slice(data);
            Poll::Ready(Ok(data.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn test_connection() -> (Arc<FcConnection>, std::sync::Arc<std::sync::Mutex<Vec<u8>>>) {
        let state = std::sync::Arc::new(Mutex::new(VehicleState::default()));
        let params = std::sync::Arc::new(Mutex::new(ParamCache::new(
            "/tmp/ados-uplink-test-params.json",
        )));
        let fc = FcConnection::new(MavlinkConfig::default(), state, params);
        (fc, std::sync::Arc::new(std::sync::Mutex::new(Vec::new())))
    }

    fn dedupe() -> Arc<RequestDedupe> {
        Arc::new(RequestDedupe::new())
    }

    /// Never completes a write, standing in for a wedged or flow-controlled FC.
    struct StallingWriter;
    impl AsyncWrite for StallingWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _data: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Pending
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Pending
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn a_decoded_uplink_frame_is_injected_into_the_fc_writer() {
        let (fc, captured) = test_connection();
        fc.connected
            .store(true, std::sync::atomic::Ordering::Relaxed);
        *fc.writer.lock().await = Some(Box::pin(CapturingWriter(captured.clone())));

        let counters = AuxUplinkConsumerCounters::new();
        let frame = heartbeat_bytes();
        let datagram = aux_mux::encode(AuxChannel::Mavlink, &frame).unwrap();

        dispatch(
            &datagram,
            &UplinkDeps {
                fc,
                egress: None,
                own_device_id: OWN_ID.into(),
                dedupe: dedupe(),
                config_tunnel: None,
            },
            &counters,
        )
        .await;

        assert_eq!(*captured.lock().unwrap(), frame);
        let snap = counters.snapshot();
        assert_eq!(snap.mavlink_frames, 1);
        assert_eq!(snap.mavlink_injected, 1);
        assert_eq!(snap.decode_foreign, 0);
        assert_eq!(snap.decode_damaged, 0);
    }

    #[tokio::test]
    async fn foreign_traffic_on_the_shared_lane_is_counted_not_alarmed() {
        let (fc, captured) = test_connection();
        fc.connected
            .store(true, std::sync::atomic::Ordering::Relaxed);
        *fc.writer.lock().await = Some(Box::pin(CapturingWriter(captured.clone())));

        let counters = AuxUplinkConsumerCounters::new();
        dispatch(
            b"not-an-aux-frame-at-all",
            &UplinkDeps {
                fc,
                egress: None,
                own_device_id: OWN_ID.into(),
                dedupe: dedupe(),
                config_tunnel: None,
            },
            &counters,
        )
        .await;

        assert!(
            captured.lock().unwrap().is_empty(),
            "foreign bytes must never reach the FC"
        );
        assert_eq!(counters.snapshot().decode_foreign, 1);
    }

    #[tokio::test]
    async fn a_batched_datagram_splits_into_separate_injections() {
        let (fc, captured) = test_connection();
        fc.connected
            .store(true, std::sync::atomic::Ordering::Relaxed);
        *fc.writer.lock().await = Some(Box::pin(CapturingWriter(captured.clone())));

        let counters = AuxUplinkConsumerCounters::new();
        let one = heartbeat_bytes();
        let mut batch = one.clone();
        batch.extend_from_slice(&one);
        let datagram = aux_mux::encode(AuxChannel::Mavlink, &batch).unwrap();

        dispatch(
            &datagram,
            &UplinkDeps {
                fc,
                egress: None,
                own_device_id: OWN_ID.into(),
                dedupe: dedupe(),
                config_tunnel: None,
            },
            &counters,
        )
        .await;

        assert_eq!(
            *captured.lock().unwrap(),
            batch,
            "both frames must reach the FC, in order"
        );
        assert_eq!(counters.snapshot().mavlink_frames, 2);
        assert_eq!(counters.snapshot().mavlink_injected, 2);
    }

    /// `ADOS_RUN_DIR` is process-global, so the sidecar tests serialise on it
    /// the same way the radio crate's sidecar tests do.
    static RUN_DIR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn feedback_datagram(fb: &ados_protocol::link_feedback::LinkFeedback) -> Vec<u8> {
        aux_mux::encode(AuxChannel::LinkFeedback, &fb.encode()).unwrap()
    }

    /// Write a config naming this node's slot, so the addressee check has
    /// something to compare against. Returns the dir to keep it alive.
    fn config_with_slot(dir: &std::path::Path, slot: u8) {
        std::fs::write(
            dir.join("config.yaml"),
            format!("video:\n  wfb:\n    fleet_slot: {slot}\n"),
        )
        .unwrap();
    }

    fn measured_feedback() -> ados_protocol::link_feedback::LinkFeedback {
        ados_protocol::link_feedback::LinkFeedback {
            loss_percent: 24.29,
            rssi_dbm: -36.0,
            snr_db: 12.4,
            packets_received: 485,
            fec_failed: 25,
            bitrate_kbps: 2242,
            has_measurement: true,
            target_slot: 1,
        }
    }

    // The env guard must span the dispatch await: it serialises a
    // process-global variable, so releasing it before the call would let a
    // sibling test retarget ADOS_RUN_DIR mid-dispatch.
    /// Point both the run dir and the config at a temp tree, and name this
    /// node's slot so the addressee check has something to compare against.
    fn stage(tag: &str, slot: u8) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ados-lfd-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        config_with_slot(&dir, slot);
        std::env::set_var("ADOS_RUN_DIR", &dir);
        std::env::set_var("ADOS_CONFIG_YAML", dir.join("config.yaml"));
        dir
    }

    fn unstage(dir: &std::path::Path) {
        std::env::remove_var("ADOS_RUN_DIR");
        std::env::remove_var("ADOS_CONFIG_YAML");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_link_feedback_report_for_this_slot_is_published_for_the_ladder() {
        let _guard = RUN_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = stage("mine", 1);

        let (fc, _captured) = test_connection();
        let counters = AuxUplinkConsumerCounters::new();
        dispatch(
            &feedback_datagram(&measured_feedback()),
            &UplinkDeps {
                fc,
                egress: None,
                own_device_id: OWN_ID.into(),
                dedupe: dedupe(),
                config_tunnel: None,
            },
            &counters,
        )
        .await;

        let snap = counters.snapshot();
        assert_eq!(snap.link_feedback_frames, 1);
        assert_eq!(snap.link_feedback_not_for_us, 0);
        assert_eq!(snap.link_feedback_write_errors, 0);
        let written =
            ados_protocol::link_feedback::read_sidecar_from(&dir.join("link-feedback.json"))
                .expect("the ladder's input must be on disk");
        assert!((written.loss_percent - 24.29).abs() < 0.01);
        assert_eq!(written.target_slot, 1);
        unstage(&dir);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn requests_beyond_the_inflight_ceiling_are_shed_not_queued() {
        // Each accepted request spawns a task that opens a local HTTP call and
        // buffers its response, so an unbounded spawn let the uplink's datagram
        // rate set the memory ceiling on a board with a few hundred megabytes.
        // Shedding is safe: the ground retransmits, and the dedupe cache
        // answers the retry from the first attempt's work.
        let _guard = RUN_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = stage("shed", 1);

        // Hold every permit, as saturated in-flight work would.
        let mut held = Vec::new();
        for _ in 0..MAX_INFLIGHT_REQUESTS {
            held.push(REQUEST_SLOTS.clone().try_acquire_owned().expect("permit"));
        }

        let (fc, _captured) = test_connection();
        let counters = AuxUplinkConsumerCounters::new();
        let payload = ados_protocol::aux_rpc::encode_request(
            ados_protocol::aux_rpc::RpcMethod::Get,
            9,
            OWN_ID.as_bytes(),
            b"/api/status",
            &[],
        )
        .unwrap();
        let datagram = aux_mux::encode(AuxChannel::Request, &payload).unwrap();
        dispatch(
            &datagram,
            &UplinkDeps {
                fc,
                egress: None,
                own_device_id: OWN_ID.into(),
                dedupe: dedupe(),
                config_tunnel: None,
            },
            &counters,
        )
        .await;

        assert_eq!(
            counters.snapshot().rpc_requests_shed,
            1,
            "a request past the ceiling must be shed rather than spawned"
        );
        drop(held);
        unstage(&dir);
    }

    // Each in-flight request can hold a full response body, so the ceiling is
    // what turns "bounded by the datagram rate" into a real number. Checked at
    // compile time so raising it past what the board can hold is a build error.
    const _: () = assert!(MAX_INFLIGHT_REQUESTS >= 1 && MAX_INFLIGHT_REQUESTS <= 16);

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_report_measuring_another_drone_is_refused() {
        // THE regression this whole change exists for. The ground transmits the
        // uplink ONCE for the whole fleet and measures only one slot, so an
        // unaddressed record handed every other drone the primary's loss — a
        // clean drone would shed video rate because a neighbour was at the edge
        // of the link. Worse than no sample, because a no-sample ladder holds.
        let _guard = RUN_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = stage("theirs", 2);

        let (fc, _captured) = test_connection();
        let counters = AuxUplinkConsumerCounters::new();
        // The record measures slot 1; this node is slot 2.
        dispatch(
            &feedback_datagram(&measured_feedback()),
            &UplinkDeps {
                fc,
                egress: None,
                own_device_id: OWN_ID.into(),
                dedupe: dedupe(),
                config_tunnel: None,
            },
            &counters,
        )
        .await;

        let snap = counters.snapshot();
        assert_eq!(snap.link_feedback_frames, 1, "it was received");
        assert_eq!(snap.link_feedback_not_for_us, 1, "and refused");
        assert!(
            ados_protocol::link_feedback::read_sidecar_from(&dir.join("link-feedback.json"))
                .is_none(),
            "another drone's measurement must never reach this drone's ladder"
        );
        unstage(&dir);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn an_unprovisioned_node_accepts_no_report() {
        // With no slot in config a node cannot prove a record is about itself,
        // and guessing is exactly the failure the addressee prevents.
        let _guard = RUN_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ados-lfd-unprov-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.yaml"),
            "video:\n  wfb:\n    channel: 149\n",
        )
        .unwrap();
        std::env::set_var("ADOS_RUN_DIR", &dir);
        std::env::set_var("ADOS_CONFIG_YAML", dir.join("config.yaml"));

        let (fc, _captured) = test_connection();
        let counters = AuxUplinkConsumerCounters::new();
        dispatch(
            &feedback_datagram(&measured_feedback()),
            &UplinkDeps {
                fc,
                egress: None,
                own_device_id: OWN_ID.into(),
                dedupe: dedupe(),
                config_tunnel: None,
            },
            &counters,
        )
        .await;

        assert_eq!(counters.snapshot().link_feedback_not_for_us, 1);
        assert!(
            ados_protocol::link_feedback::read_sidecar_from(&dir.join("link-feedback.json"))
                .is_none()
        );
        unstage(&dir);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_damaged_link_feedback_report_is_counted_and_not_published() {
        let _guard = RUN_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ados-lfd-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("ADOS_RUN_DIR", &dir);

        let (fc, _captured) = test_connection();
        let counters = AuxUplinkConsumerCounters::new();
        // Truncated record: decoding it as zeros would hand the ladder an
        // artificially clean link and push the rate the wrong way.
        let truncated = aux_mux::encode(AuxChannel::LinkFeedback, &[1u8, 0, 0, 0]).unwrap();
        dispatch(
            &truncated,
            &UplinkDeps {
                fc,
                egress: None,
                own_device_id: OWN_ID.into(),
                dedupe: dedupe(),
                config_tunnel: None,
            },
            &counters,
        )
        .await;

        let snap = counters.snapshot();
        assert_eq!(snap.link_feedback_frames, 1);
        assert_eq!(snap.link_feedback_undecodable, 1);
        assert!(
            ados_protocol::link_feedback::read_sidecar_from(&dir.join("link-feedback.json"))
                .is_none(),
            "a damaged report must never reach the ladder"
        );

        std::env::remove_var("ADOS_RUN_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_request_addressed_to_another_drone_is_dropped_unanswered() {
        let (fc, _captured) = test_connection();
        let counters = AuxUplinkConsumerCounters::new();
        let payload = ados_protocol::aux_rpc::encode_request(
            ados_protocol::aux_rpc::RpcMethod::Get,
            1,
            b"deadbeefcafe",
            b"/api/pairing/info",
            &[],
        )
        .unwrap();
        let datagram = aux_mux::encode(AuxChannel::Request, &payload).unwrap();

        dispatch(
            &datagram,
            &UplinkDeps {
                fc,
                egress: None,
                own_device_id: OWN_ID.into(),
                dedupe: dedupe(),
                config_tunnel: None,
            },
            &counters,
        )
        .await;

        let snap = counters.snapshot();
        assert_eq!(snap.rpc_requests, 1);
        assert_eq!(
            snap.rpc_requests_not_for_us, 1,
            "answering another drone's request would hand the ground the wrong node's data"
        );
    }

    #[tokio::test]
    async fn a_request_for_us_or_broadcast_is_accepted() {
        for target in [OWN_ID.as_bytes(), b""] {
            let (fc, _captured) = test_connection();
            let counters = AuxUplinkConsumerCounters::new();
            let payload = ados_protocol::aux_rpc::encode_request(
                ados_protocol::aux_rpc::RpcMethod::Get,
                1,
                target,
                b"/api/pairing/info",
                &[],
            )
            .unwrap();
            let datagram = aux_mux::encode(AuxChannel::Request, &payload).unwrap();

            dispatch(
                &datagram,
                &UplinkDeps {
                    fc,
                    egress: None,
                    own_device_id: OWN_ID.into(),
                    dedupe: dedupe(),
                    config_tunnel: None,
                },
                &counters,
            )
            .await;

            let snap = counters.snapshot();
            assert_eq!(snap.rpc_requests, 1);
            assert_eq!(snap.rpc_requests_not_for_us, 0, "target {target:?}");
        }
    }

    /// The regression every other target test misses: they all fix `OWN_ID`,
    /// but a drone with no `/etc/ados/device-id` reports an empty own id and
    /// used to drop every named request — 100% failure on an unprovisioned
    /// node, while the startup log claimed the opposite.
    #[tokio::test]
    async fn a_named_request_is_accepted_when_our_own_id_is_unresolved() {
        let (fc, _captured) = test_connection();
        let counters = AuxUplinkConsumerCounters::new();
        let payload = ados_protocol::aux_rpc::encode_request(
            ados_protocol::aux_rpc::RpcMethod::Get,
            1,
            b"deadbeefcafe",
            b"/api/pairing/info",
            &[],
        )
        .unwrap();
        let datagram = aux_mux::encode(AuxChannel::Request, &payload).unwrap();

        dispatch(
            &datagram,
            &UplinkDeps {
                fc,
                egress: None,
                own_device_id: "".into(),
                dedupe: dedupe(),
                config_tunnel: None,
            },
            &counters,
        )
        .await;

        let snap = counters.snapshot();
        assert_eq!(snap.rpc_requests, 1);
        assert_eq!(
            snap.rpc_requests_not_for_us, 0,
            "an unresolved local id cannot adjudicate a target, so it must fail open"
        );
    }

    /// The recv loop that feeds the relay-proxy Request lane must not park
    /// behind a flight controller that has stopped accepting writes.
    #[tokio::test(start_paused = true)]
    async fn a_wedged_fc_write_is_bounded_and_counted() {
        let (fc, _captured) = test_connection();
        fc.connected
            .store(true, std::sync::atomic::Ordering::Relaxed);
        *fc.writer.lock().await = Some(Box::pin(StallingWriter));

        let counters = AuxUplinkConsumerCounters::new();
        let datagram = aux_mux::encode(AuxChannel::Mavlink, &heartbeat_bytes()).unwrap();

        dispatch(
            &datagram,
            &UplinkDeps {
                fc,
                egress: None,
                own_device_id: OWN_ID.into(),
                dedupe: dedupe(),
                config_tunnel: None,
            },
            &counters,
        )
        .await;

        let snap = counters.snapshot();
        assert_eq!(snap.mavlink_write_timeouts, 1);
        assert_eq!(
            snap.mavlink_injected, 0,
            "a write that never completed is not an injection"
        );
    }

    /// The drone half of the config-over-radio bridge: a request from the ground
    /// station is handed to the local config-tunnel service with its aux framing
    /// intact, and never touches the flight controller.
    #[tokio::test]
    async fn a_config_tunnel_frame_is_forwarded_to_the_local_service_and_not_to_the_fc() {
        use ados_protocol::config_tunnel_ingest::ConfigTunnelIngest;

        let (fc, captured) = test_connection();
        fc.connected
            .store(true, std::sync::atomic::Ordering::Relaxed);
        *fc.writer.lock().await = Some(Box::pin(CapturingWriter(captured.clone())));

        // Stands in for the config-tunnel service's own loopback ingress.
        let service = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let tunnel = ConfigTunnelIngest::new(service.local_addr().unwrap().port());

        let counters = AuxUplinkConsumerCounters::new();
        let datagram = aux_mux::encode(AuxChannel::ConfigTunnel, b"a-tunnel-frame").unwrap();
        dispatch(
            &datagram,
            &UplinkDeps {
                fc,
                egress: None,
                own_device_id: OWN_ID.into(),
                dedupe: dedupe(),
                config_tunnel: Some(Arc::new(tunnel)),
            },
            &counters,
        )
        .await;

        let mut buf = [0u8; 256];
        let (n, _) = service.recv_from(&mut buf).await.unwrap();
        assert_eq!(
            &buf[..n],
            datagram.as_slice(),
            "framing must survive the hop"
        );
        assert!(
            captured.lock().unwrap().is_empty(),
            "a config frame must never reach the flight controller"
        );

        let snap = counters.snapshot();
        assert_eq!(snap.config_tunnel_frames, 1);
        assert_eq!(snap.config_tunnel_forwarded, 1);
        assert_eq!(snap.config_tunnel_undelivered, 0);
        assert_eq!(snap.mavlink_injected, 0);
    }

    /// The channel is opt-in, so most drones run no config-tunnel service. Such
    /// a frame is counted and dropped rather than read as a fault — and it still
    /// must not reach the FC.
    #[tokio::test]
    async fn a_config_tunnel_frame_with_no_local_service_is_counted_not_lost() {
        let (fc, captured) = test_connection();
        fc.connected
            .store(true, std::sync::atomic::Ordering::Relaxed);
        *fc.writer.lock().await = Some(Box::pin(CapturingWriter(captured.clone())));

        let counters = AuxUplinkConsumerCounters::new();
        let datagram = aux_mux::encode(AuxChannel::ConfigTunnel, b"a-tunnel-frame").unwrap();
        dispatch(
            &datagram,
            &UplinkDeps {
                fc,
                egress: None,
                own_device_id: OWN_ID.into(),
                dedupe: dedupe(),
                config_tunnel: None,
            },
            &counters,
        )
        .await;

        assert!(captured.lock().unwrap().is_empty());
        let snap = counters.snapshot();
        assert_eq!(snap.config_tunnel_frames, 1);
        assert_eq!(snap.config_tunnel_undelivered, 1);
        assert_eq!(
            snap.non_mavlink_channel, 0,
            "a known channel must not fall through to the unknown-channel tally"
        );
    }
}
