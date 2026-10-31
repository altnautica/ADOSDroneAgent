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
//! link. Every other channel is dispatched to its own sink where one is
//! registered, and counted and dropped where none is.
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

use ados_protocol::shutdown::Shutdown;
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

mod counters;
mod dispatch;
mod run;
mod seam;
use counters::report;
use dispatch::dispatch;
pub use run::{run_aux_consumer, supervise_aux_consumer};

/// Largest datagram read in one go.
///
/// An aux frame is capped well below this by the framing itself; the headroom
/// means an oversized datagram is read and rejected by the decoder rather than
/// silently truncated by the read and then misparsed as a damaged frame of ours.
const BUF_SIZE: usize = 4096;

/// Fixed delay before the republish seam is tried again after it refused a
/// connection.
///
/// Flat, with no growth: every frame dropped while the gate is closed is
/// relayed telemetry or a command ack the operator does not see, so a router
/// that came back must be picked up within seconds, not after a ladder that
/// had doubled to half a minute. The interval still keeps a router that is
/// down from facing one connection attempt per frame.
const SEAM_RETRY_INTERVAL: Duration = Duration::from_secs(2);

/// Fixed retry between attempts to bind the lane's loopback port.
///
/// Flat, with no ceiling and no attempt cap: a bind that keeps failing is a
/// consumer that receives nothing, and the ladder this replaces doubled to 30 s
/// before each further attempt. The interval floor is what keeps a hard-failing
/// bind from busy-spinning.
const BIND_RETRY_INTERVAL: Duration = Duration::from_secs(5);

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
    /// Frames whose MAVLink system id was already being presented by a
    /// DIFFERENT fleet slot. Non-zero means two aircraft are one vehicle to
    /// anything downstream, and a command addressed to that id is accepted by
    /// both -- so this is a safety signal, not a statistic.
    system_id_collisions: AtomicU64,
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
    /// Peer identity frames whose advertised plugin contract version diverges
    /// from this node's own for a plugin both run. A non-zero count is the
    /// operator-facing signal that an upgrade-mixed pair will disagree on the
    /// plugin wire; a SUB-TALLY of `identity_frames`, so it is deliberately
    /// absent from [`AuxCountersSnapshot::accounted`].
    identity_plugin_version_mismatch: AtomicU64,
    /// Datagrams that decoded as the relay-proxy Response channel. Routed to the
    /// registered proxy if any; otherwise counted and dropped. A ground rig
    /// running no proxy caller still receives these when a peer buildAnswers
    /// requests the ground sends, and silence would hide the lane is alive.
    response_frames: AtomicU64,
    /// Datagrams that decoded as the relay-proxy Request channel. The ground
    /// rig never receives this channel — Requests only travel ground→drone —
    /// so a non-zero count means a peer is sending the wrong direction or a
    /// frame was misrouted. Counted for visibility, not alarmed on.
    request_frames: AtomicU64,
    /// Datagrams that decoded as the link-feedback channel. A ground station is
    /// the SENDER of this channel, never a receiver, so a non-zero count means a
    /// peer is sending the wrong direction or a frame was misrouted. Counted for
    /// visibility, not alarmed on — same posture as `request_frames`.
    link_feedback_frames: AtomicU64,
    /// Datagrams that decoded as the config-over-radio channel: the linked
    /// drone's answer to a config request this ground station sent. Forwarded to
    /// the local config-tunnel service, which is inert on a node that never
    /// opted the channel in.
    config_tunnel_frames: AtomicU64,
    /// Config-tunnel datagrams this hop handed to the local ingress socket
    /// without error. Sub-tally of `config_tunnel_frames`, and the pair to
    /// `config_tunnel_undelivered`: between them every one is accounted for.
    ///
    /// It does NOT mean the tunnel service read them. The ingress is an
    /// unconnected UDP socket, deliberately, so that an absent listener cannot
    /// latch an error and poison the next forward once the service starts. The
    /// cost of that choice is that a send to a port nothing is bound to still
    /// succeeds, so on a node with the channel off this counter advances and
    /// `config_tunnel_undelivered` stays at zero. Proof that the service
    /// consumed a frame lives on the service's own side, not here.
    config_tunnel_forwarded: AtomicU64,
    /// Config-tunnel datagrams the hand-off itself failed on: the ingress
    /// socket could not be bound, or the send returned an error. Sub-tally of
    /// `config_tunnel_frames`. Rare, and not the same thing as the service
    /// being absent, which this cannot see.
    config_tunnel_undelivered: AtomicU64,
    /// Response datagrams whose body did not decode as a relay-proxy response
    /// payload. Sub-tally of `response_frames`, not a terminal outcome, so it
    /// is deliberately absent from [`AuxCountersSnapshot::accounted`].
    response_undecodable: AtomicU64,
    /// Response datagrams forwarded to the relay proxy's ingest. Sub-tally of
    /// `response_frames`, and the pair to `response_orphan`: between them every
    /// decoded Response is accounted for.
    response_dispatched: AtomicU64,
    /// Response datagrams whose id matched no pending caller. Normal in small
    /// numbers on a reordering lane (the caller timed out and removed itself
    /// between send and response). Sub-tally of `response_frames`.
    response_orphan: AtomicU64,
    /// Datagrams that decoded as the neutral application-stream channel,
    /// drone → ground. Routed to the registered application sink if any;
    /// otherwise counted and dropped so a rig running no consumer can still
    /// tell the lane is alive.
    app_stream: AtomicU64,
    /// AppStream datagrams dropped for want of a registered sink. Sub-tally of
    /// `app_stream`, the pair that between them accounts for every one received.
    app_stream_undelivered: AtomicU64,
    /// Datagrams that decoded as the application-command channel. That channel
    /// is an uplink (ground → drone); a ground downlink consumer never
    /// legitimately receives one, so a non-zero count means a peer sent the
    /// wrong direction or a frame was misrouted. Counted and dropped, never
    /// forwarded.
    app_command_unexpected: AtomicU64,
    recv_errors: AtomicU64,
    bind_failures: AtomicU64,
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
    /// Datagrams that decoded as the relay-proxy Response channel. Routed to
    /// the registered proxy if any, otherwise counted and dropped.
    pub response_frames: u64,
    /// Datagrams that decoded as the relay-proxy Request channel. The ground
    /// never receives this channel — Requests travel ground→drone only — so a
    /// non-zero count means a peer is sending the wrong direction or a frame
    /// was misrouted. Counted for visibility, not alarmed on.
    pub request_frames: u64,
    pub link_feedback_frames: u64,
    /// Datagrams that decoded as the config-over-radio channel, forwarded to
    /// the local config-tunnel service.
    pub config_tunnel_frames: u64,
    /// Config-tunnel datagrams the local service's ingress took. Sub-tally of
    /// `config_tunnel_frames`.
    pub config_tunnel_forwarded: u64,
    /// Config-tunnel datagrams the hand-off did not take — the normal case with
    /// the channel opted out. Sub-tally of `config_tunnel_frames`.
    pub config_tunnel_undelivered: u64,
    /// MAVLink frames the republish seam accepted. Not proof a ground control
    /// station was connected or rendered them.
    pub mavlink_republished: u64,
    pub system_id_collisions: u64,
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
    /// Peer identity frames whose advertised plugin contract version diverges
    /// from this node's own for a plugin both run. Sub-tally of
    /// `identity_frames`; the upgrade-mismatch signal.
    pub identity_plugin_version_mismatch: u64,
    /// Response datagrams whose body did not decode as a relay-proxy response.
    /// Sub-tally of `response_frames`.
    pub response_undecodable: u64,
    /// Response datagrams forwarded to the relay proxy's ingest. Sub-tally of
    /// `response_frames`.
    pub response_dispatched: u64,
    /// Response datagrams whose id matched no pending caller. Sub-tally of
    /// `response_frames`.
    pub response_orphan: u64,
    /// Datagrams that decoded as the neutral application-stream channel,
    /// drone → ground. Routed to the registered sink if any, else counted and
    /// dropped.
    pub app_stream: u64,
    /// AppStream datagrams dropped for want of a registered sink. Sub-tally of
    /// `app_stream`.
    pub app_stream_undelivered: u64,
    /// Datagrams that decoded as the application-command channel — an uplink a
    /// ground downlink consumer never legitimately receives. Counted and
    /// dropped.
    pub app_command_unexpected: u64,
    /// Failed reads on the loopback socket.
    pub recv_errors: u64,
    /// Failed attempts to bind the lane's loopback port. A non-zero value with
    /// nothing received means the consumer never started listening at all.
    pub bind_failures: u64,
}

/// Retry gate for the republish seam.
///
/// Holds the retry schedule so a MAVLink router that is not up is retried on a
/// fixed interval rather than on every single frame, which at telemetry rates
/// would be a connection storm against a process that is already struggling.
#[derive(Debug, Default)]
struct SeamGate {
    /// When the next attempt may be made. `None` means immediately.
    next_attempt: Option<Instant>,
    /// Whether the current outage has been logged, so a long outage produces one
    /// line rather than one per retry.
    outage_logged: bool,
}

/// A sink for the neutral application-stream channel ([`AuxChannel::AppStream`]).
///
/// Registered by whatever service consumes application datagrams a paired drone
/// radiates; `None` on a rig running no such consumer, where an AppStream frame
/// is counted and dropped rather than silently swallowed.
pub trait AppStreamSink: Send + Sync {
    fn deliver(&self, payload: &[u8]);
}

/// Where a decoded aux frame goes, chosen by its channel.
///
/// Grouped rather than threaded one by one: every layer of the consumer carries
/// the whole set, so each new lane widened three signatures at once and pushed
/// them past the argument limit. Adding a lane is now one field.
pub struct AuxSinks<'a> {
    pub mavlink: &'a MavlinkIngest,
    pub rpc_response: Option<&'a ados_protocol::aux_rpc_proxy::AuxRpcResponseIngest>,
    pub config_tunnel: Option<&'a ados_protocol::config_tunnel_ingest::ConfigTunnelIngest>,
    pub app_stream: Option<&'a dyn AppStreamSink>,
    /// This node's own installed plugins and their contract versions, so the
    /// identity consumer can flag a peer advertising a different version for a
    /// plugin both run. `None` where no comparison is wanted.
    pub local_plugins: Option<&'a std::collections::HashMap<String, u16>>,
}

/// The owned form the service holds, cloned per supervised generation.
#[derive(Clone)]
pub struct AuxSinksOwned {
    pub mavlink: Arc<MavlinkIngest>,
    pub rpc_response: Option<ados_protocol::aux_rpc_proxy::AuxRpcResponseIngest>,
    pub config_tunnel: Option<Arc<ados_protocol::config_tunnel_ingest::ConfigTunnelIngest>>,
    pub app_stream: Option<Arc<dyn AppStreamSink>>,
    pub local_plugins: Option<Arc<std::collections::HashMap<String, u16>>>,
}

#[cfg(test)]
mod tests;
