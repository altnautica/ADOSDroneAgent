//! Ground-side client for relay-proxy HTTP-over-aux calls.
//!
//! A ground station paired to a drone only over WFB has no IP reach to that
//! drone, but the aux lane already carries MAVLink, status, and identity low-
//! rate frames between them. This module rides a fourth and fifth channel on
//! the same pair — `Request` (ground→drone, uplink, radio_id 3) and `Response`
//! (drone→ground, downlink, radio_id 2) — to forward an HTTP request and
//! return its response, so a `relay-proxy` HTTP route on the ground station can
//! reach the drone's own agent HTTP API for a peer it has no LAN or cloud
//! address for.
//!
//! ## Process boundary
//!
//! The aux consumer (which receives Response frames off the radio) runs in the
//! ground-station data-plane process, and the HTTP route handler lives in the
//! control-surface process. They are separate processes, so the proxy's pending
//! map cannot be shared in-process. The seam between them is a Unix domain
//! socket (`/run/ados/aux-rpc-responses.sock`): the proxy's reader LISTENS on
//! that socket, and the consumer's ingest writer CONNECTS and forwards each
//! Response payload as a length-prefixed frame. This mirrors the existing
//! `MavlinkIngest` pattern exactly — same framing, same lifecycle, same
//! bounded-failure posture.
//!
//! ## Correlation
//!
//! Each request carries a 32-bit id assigned by the proxy. The drone's
//! response echoes it back unchanged, so a pending-request map on the ground
//! matches a response to its caller without ordering assumptions across a
//! reordering datagram lane. A solo proxy caller can use a monotonic counter;
//! the 32-bit space does not roll over in any realistic session at a few HTTP
//! calls per second.
//!
//! ## Bounded failure
//!
//! Every call is bounded by [`RPC_DEFAULT_TIMEOUT`]. A timeout removes the
//! pending entry and returns [`RpcError::Timeout`] to the caller rather than
//! parking any worker on the radio. A drone that never answers — radio dead,
//! consumer not running, handler not registered — surfaces as a bounded
//! failure on the very first call, which is what its HTTP caller needs to
//! report to the operator.
//!
//! ## What a successful call does and does not prove
//!
//! `Ok` means the drone decoded the request, dispatched it against its own
//! HTTP API, and returned a response with the same request id. It does NOT
//! prove the drone's HTTP server returned 200; the response's HTTP status
//! travels inside the payload and may be 404, 500, or anything else the
//! drone's own API returned. Callers must inspect [`RpcResponseOwned::status`].
//!
//! ## Send-side reuse
//!
//! The proxy holds one [`AuxEgress`] for its lifetime and reuses it across
//! calls. The egress opens the aux pair lazily on first send and re-opens on
//! a dead socket, so a proxy created before the radio is ready is not a
//! problem; the first call's open is what lights the uplink up.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::aux_egress::AuxEgress;
use crate::aux_mux::{self, AuxChannel};
use crate::aux_rpc::{self, RpcMethod};
use crate::frame::HEADER_SIZE;
use crate::ipc;
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{oneshot, Mutex, Notify};

mod call;
mod dispatch;
mod error;
mod listener;
mod stats;
pub use error::*;
use listener::*;
pub use stats::*;

/// Default per-call timeout.
///
/// A round trip is one uplink datagram, the drone's own HTTP processing, and
/// as many downlink fragments as the response needs. A 25-fragment response
/// paced at 5 ms plus radio RTT plus the drone's own 5-second HTTP bound does
/// not fit inside 5 seconds, so the ground bound is 10.
///
/// **The two bounds must not be equal.** The ground bound has to exceed the
/// drone's `HTTP_TIMEOUT` so a wedged drone API surfaces as the drone's own
/// 503 rather than an ambiguous ground-side timeout that says nothing about
/// which hop failed.
pub const RPC_DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a write to the proxy's local ingest socket may take.
///
/// This is a loopback Unix socket, so a healthy peer completes in microseconds
/// and anything near this bound means the peer has stopped reading. The caller
/// awaits it inside the aux consumer's single read loop, so the bound is what
/// keeps one wedged proxy from stalling a whole drone's inbound traffic.
const IPC_WRITE_TIMEOUT: Duration = Duration::from_millis(200);

/// How long connecting to that socket may take. A listener that is present
/// accepts immediately; one that is absent fails immediately. A long wait here
/// means neither, which is not worth a read loop.
const IPC_CONNECT_TIMEOUT: Duration = Duration::from_millis(200);

/// Resend schedule for the Request datagram, as gaps between attempts.
///
/// The drone's radio is half-duplex and spends ~60% of its airtime injecting
/// video, so an uplink datagram that lands inside a TX burst is lost at the
/// PHY before it ever reaches a socket. Video frames pace at ~33 ms (30 fps)
/// and bursts run ~10-20 ms, so these gaps are deliberately non-harmonic with
/// the frame period and each is well clear of a single burst; the growth
/// escapes a multi-frame fade. 5 attempts at a measured ~35% per-attempt loss
/// leaves ~0.5% residual.
///
/// Retransmission is only safe because the drone deduplicates on request id
/// and replays its cached response instead of re-executing the HTTP call.
const RPC_RETRY_DELAYS: [Duration; 4] = [
    Duration::from_millis(150),
    Duration::from_millis(250),
    Duration::from_millis(400),
    Duration::from_millis(700),
];

/// Most concurrent in-flight calls. Beyond this the proxy sheds load rather
/// than queueing radio work it cannot deliver.
///
/// A fleet-wide operation is 24 targeted calls issued concurrently (there is
/// no broadcast fan-in; see [`AuxRpcProxy::call`]), and an operator surface
/// polling several endpoints per drone multiplies that again. 256 was sized
/// for one linked drone and a 24-drone poll would exhaust it, turning a
/// working lane into a wall of `Busy`.
const MAX_PENDING_CALLS: usize = 1024;

/// Pause between the fragments of one multi-fragment request, for the same
/// reason the drone paces its response fragments: back-to-back datagrams into
/// the transmitter's UDP ingress can overrun its socket buffer and vanish.
const REQUEST_FRAGMENT_PACING: Duration = Duration::from_millis(5);

/// The default Unix socket path for the Response IPC seam.
pub const DEFAULT_RESPONSE_SOCK: &str = "/run/ados/aux-rpc-responses.sock";

/// One in-flight call: the caller waiting on it, plus the fragments that have
/// arrived so far.
struct PendingCall {
    sender: oneshot::Sender<RpcResponseOwned>,
    /// The device id this call was addressed to; `None` for a broadcast.
    ///
    /// One fleet key covers up to 24 drones, so a request id is only unique
    /// per sender. A fragment from any other aircraft is a different answer,
    /// and RaptorQ has no per-object checksum that would catch the splice —
    /// the caller would get a cleanly-decoded body belonging to neither drone.
    expected_sender: Option<Box<str>>,
    /// Seeded by the first usable fragment that arrives; `None` until then.
    assembly: Option<Assembly>,
    /// Set by the first Response fragment carrying this id *from the expected
    /// sender*, whether or not it was usable. Any such fragment proves the
    /// Request datagram reached the drone, so it is the stop signal for
    /// [`RPC_RETRY_DELAYS`]: resending after it would only duplicate work the
    /// drone has already done. Another drone's fragment proves nothing about
    /// ours, so it deliberately does not set this.
    response_started: bool,
}

/// Partial reassembly state for one response.
struct Assembly {
    status: u16,
    total: u16,
    /// Symbols accepted so far. Only meaningful as the `received` half of an
    /// [`RpcError::Incomplete`] report: the call completes on whatever count
    /// RaptorQ finds sufficient, not on reaching `total`.
    received: usize,
    /// Fragment indices already fed to the decoder. The fragment ceiling
    /// ([`aux_rpc::MAX_RESPONSE_FRAGMENTS`]) is 64, so the whole set is one
    /// word and a re-delivered fragment costs a bit test rather than a scan.
    accepted: u64,
    decoder: aux_rpc::ResponseDecoder,
}

/// Lane health for one proxy. `AuxRpcProxy` had no counters at all, so a
/// failing relay was invisible to everything except the single HTTP caller
/// that happened to be waiting on it.
#[derive(Default)]
pub struct RpcProxyCounters {
    calls_started: AtomicU64,
    calls_ok: AtomicU64,
    calls_timeout: AtomicU64,
    calls_incomplete: AtomicU64,
    calls_send_error: AtomicU64,
    calls_channel_closed: AtomicU64,
    calls_busy: AtomicU64,
    retransmits: AtomicU64,
    /// Retransmits that could not leave: a broken egress rather than an
    /// unanswering far end.
    retransmit_send_errors: AtomicU64,
    fragments_received: AtomicU64,
    fragments_duplicate: AtomicU64,
    /// Fragments dropped because the drone that sent them is not the one this
    /// call addressed. Non-zero means two aircraft answered one id, which on a
    /// shared fleet key is the failure that would otherwise splice two bodies.
    fragments_wrong_sender: AtomicU64,
    /// Fragments dropped because the symbol was not the size or id this
    /// response's geometry allows: radio damage, or the two ends disagreeing
    /// on the wire layout across an upgrade.
    fragments_bad_symbol: AtomicU64,
    responses_out_of_range: AtomicU64,
    /// Live pending-map size, tracked as its own atomic rather than read as a
    /// map length so [`AuxRpcProxy::stats`] stays synchronous and a status
    /// handler never has to await the pending mutex.
    pending_now: AtomicU64,
}

/// One pending caller, keyed by request id. Cheap to clone; every clone shares
/// the same pending map and egress.
#[derive(Clone)]
pub struct AuxRpcProxy {
    egress: Arc<AuxEgress>,
    pending: Arc<Mutex<HashMap<u32, PendingCall>>>,
    next_id: Arc<AtomicU32>,
    counters: Arc<RpcProxyCounters>,
    timeout: Duration,
}

impl AuxRpcProxy {
    /// A proxy using the given egress, with the default call timeout.
    pub fn new(egress: AuxEgress) -> Self {
        Self::with_timeout(egress, RPC_DEFAULT_TIMEOUT)
    }

    /// A proxy with an explicit per-call timeout (tests use a short one so a
    /// no-response case does not cost seconds).
    pub fn with_timeout(egress: AuxEgress, timeout: Duration) -> Self {
        // A restart must not reuse the id space a still-in-flight response
        // belongs to: correlation is the bare id with no echo of the request,
        // so a collision silently hands one caller another call's body.
        // Seeding from the clock makes reuse within a response window
        // vanishingly unlikely without a wire change.
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() ^ std::process::id())
            .unwrap_or(1)
            | 1;
        Self {
            egress: Arc::new(egress),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU32::new(seed)),
            counters: Arc::new(RpcProxyCounters::default()),
            timeout,
        }
    }

    /// Drop every pending caller. Used on shutdown so a caller awaiting a
    /// response whose consumer is gone does not wait the full timeout.
    pub async fn reset(&self) {
        let mut pending = self.pending.lock().await;
        for (_, call) in pending.drain() {
            let _ = call.sender.send(RpcResponseOwned {
                status: 503,
                body: Vec::new(),
                headers: Vec::new(),
            });
        }
        self.counters.pending_now.store(0, Ordering::Relaxed);
    }

    /// Spawn the reader task that LISTENS on the Response IPC socket and
    /// dispatches each frame to its pending caller. Returns a cancel handle;
    /// notifying it stops the reader cleanly.
    ///
    /// The listener accepts connections from the consumer's ingest writer (one
    /// per consumer process). A consumer that connects after the proxy is up
    /// is accepted immediately; a consumer that disconnects mid-session has its
    /// connection dropped and a new one accepted on the next reconnect.
    pub fn spawn_response_listener(
        &self,
        sock_path: impl Into<PathBuf>,
        cancel: Arc<Notify>,
    ) -> tokio::task::JoinHandle<()> {
        let proxy = self.clone();
        let sock_path = sock_path.into();
        tokio::spawn(async move {
            run_response_listener(proxy, sock_path, cancel).await;
        })
    }
}

/// Removes its pending entry when dropped, however the call ends — including a
/// cancelled future, which ordinary post-await cleanup never reaches. An axum
/// handler whose client disconnects drops the call future mid-flight, and
/// without this the entry (with its `oneshot::Sender` and up to a 76 KB
/// `Assembly`) leaks for the life of the process.
///
/// The map is behind an async mutex, so the removal is scheduled rather than
/// taken inline; `Handle::try_current` keeps a drop outside the runtime from
/// panicking.
struct PendingGuard {
    pending: Arc<Mutex<HashMap<u32, PendingCall>>>,
    counters: Arc<RpcProxyCounters>,
    id: u32,
    armed: bool,
}

/// Writer side: CONNECTS to the Response IPC socket and writes each Response
/// payload as a length-prefixed frame. Used by the aux consumer's process to
/// forward Response datagrams to the proxy's process.
#[derive(Clone)]
pub struct AuxRpcResponseIngest {
    sock_path: PathBuf,
    conn: Arc<Mutex<Option<UnixStream>>>,
}

#[cfg(test)]
mod tests;
