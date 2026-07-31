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
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{oneshot, Mutex, Notify};

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

/// The default Unix socket path for the Response IPC seam.
pub const DEFAULT_RESPONSE_SOCK: &str = "/run/ados/aux-rpc-responses.sock";

/// The response, owned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcResponseOwned {
    /// The HTTP status code the drone's API returned.
    pub status: u16,
    /// The HTTP response body bytes the drone's API returned, reassembled from
    /// every fragment.
    pub body: Vec<u8>,
}

/// Why a call did not complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcError {
    /// The request would not encode (oversized for one aux frame). The HTTP
    /// caller should surface a 413.
    Encode,
    /// The aux egress failed to send the request datagram.
    Send(String),
    /// No response arrived before the bound elapsed.
    Timeout,
    /// Some fragments arrived and the rest did not. Distinct from `Timeout`
    /// on purpose: "the drone never answered" and "the radio dropped a
    /// fragment" are different faults with different operator actions.
    Incomplete { received: usize, total: u16 },
    /// The consumer torn down between the request and the response, so the
    /// pending entry was removed. The caller may retry.
    ChannelClosed,
    /// Too many calls already in flight. Shedding here is honest: the lane
    /// cannot deliver them, and queueing would only convert a fast 503 into a
    /// slow timeout.
    Busy,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Encode => write!(f, "request payload exceeds one aux frame"),
            Self::Send(e) => write!(f, "aux egress send failed: {e}"),
            Self::Timeout => write!(f, "no response before timeout"),
            Self::Incomplete { received, total } => {
                write!(f, "relay response incomplete: {received}/{total} fragments")
            }
            Self::ChannelClosed => write!(f, "aux consumer channel closed before response"),
            Self::Busy => write!(f, "relay proxy has too many calls in flight"),
        }
    }
}

impl std::error::Error for RpcError {}

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

/// A point-in-time read of [`RpcProxyCounters`], shaped for the status route.
#[derive(Debug, Default, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct RpcProxyStats {
    pub calls_started: u64,
    pub calls_ok: u64,
    pub calls_timeout: u64,
    pub calls_incomplete: u64,
    pub calls_send_error: u64,
    pub calls_channel_closed: u64,
    pub calls_busy: u64,
    pub retransmits: u64,
    pub fragments_received: u64,
    pub fragments_duplicate: u64,
    pub fragments_wrong_sender: u64,
    pub fragments_bad_symbol: u64,
    pub responses_out_of_range: u64,
    pub pending_now: u64,
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

    /// Forward one HTTP-shaped request to the drone and await its response.
    ///
    /// `target` is the device id of the drone this call is for; an empty slice
    /// broadcasts to every linked drone. `path` is the absolute path on the
    /// drone's own HTTP API, including the leading slash and any query string
    /// (e.g. `/api/logs?limit=5`). `body` is the request body (empty for GET).
    /// The drone's HTTP server returns the status and body; the proxy does not
    /// interpret either.
    pub async fn call(
        &self,
        target: &[u8],
        method: RpcMethod,
        path: &[u8],
        body: &[u8],
    ) -> Result<RpcResponseOwned, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.call_with_id(id, target, method, path, body).await
    }

    async fn call_with_id(
        &self,
        id: u32,
        target: &[u8],
        method: RpcMethod,
        path: &[u8],
        body: &[u8],
    ) -> Result<RpcResponseOwned, RpcError> {
        let payload =
            aux_rpc::encode_request(method, id, target, path, body).ok_or(RpcError::Encode)?;
        // An empty target is a broadcast, which any linked drone may answer,
        // so there is no single sender to hold its fragments to. A target that
        // is not UTF-8 is not a device id any drone will ever send back, and
        // the lossy conversion makes that mismatch — the safe direction.
        let expected_sender = (!target.is_empty()).then(|| {
            String::from_utf8_lossy(target)
                .into_owned()
                .into_boxed_str()
        });

        let (tx, rx) = oneshot::channel::<RpcResponseOwned>();
        {
            let mut pending = self.pending.lock().await;
            if pending.len() >= MAX_PENDING_CALLS {
                self.counters.calls_busy.fetch_add(1, Ordering::Relaxed);
                return Err(RpcError::Busy);
            }
            pending.insert(
                id,
                PendingCall {
                    sender: tx,
                    expected_sender,
                    assembly: None,
                    response_started: false,
                },
            );
        }
        self.counters.pending_now.fetch_add(1, Ordering::Relaxed);
        self.counters.calls_started.fetch_add(1, Ordering::Relaxed);

        // Armed from here on: every exit below either removes the entry itself
        // and disarms, or is a cancellation the guard has to clean up for us.
        let mut guard = PendingGuard {
            pending: Arc::clone(&self.pending),
            counters: Arc::clone(&self.counters),
            id,
            armed: true,
        };

        if let Err(e) = self.egress.send(AuxChannel::Request, &payload).await {
            guard.disarm();
            self.remove_pending(id).await;
            self.counters
                .calls_send_error
                .fetch_add(1, Ordering::Relaxed);
            return Err(RpcError::Send(e.to_string()));
        }

        // One datagram on a lane that loses 20-40% of the uplink is a coin
        // flip, so resend on a growing schedule until either the response
        // starts arriving or the caller's bound elapses.
        let deadline = tokio::time::Instant::now() + self.timeout;
        let mut attempt = 0usize;
        tokio::pin!(rx);

        loop {
            let wake = match RPC_RETRY_DELAYS.get(attempt).copied() {
                Some(d) => (tokio::time::Instant::now() + d).min(deadline),
                None => deadline,
            };
            tokio::select! {
                biased;
                res = &mut rx => {
                    guard.disarm();
                    return match res {
                        Ok(resp) => {
                            self.counters.calls_ok.fetch_add(1, Ordering::Relaxed);
                            Ok(resp)
                        }
                        Err(_) => {
                            self.remove_pending(id).await;
                            self.counters
                                .calls_channel_closed
                                .fetch_add(1, Ordering::Relaxed);
                            Err(RpcError::ChannelClosed)
                        }
                    };
                }
                _ = tokio::time::sleep_until(wake) => {
                    if tokio::time::Instant::now() >= deadline {
                        guard.disarm();
                        // Some fragments in flight is a different fault from silence.
                        let partial = self.remove_pending(id).await;
                        return match partial.and_then(|p| p.assembly) {
                            Some(a) => {
                                self.counters
                                    .calls_incomplete
                                    .fetch_add(1, Ordering::Relaxed);
                                Err(RpcError::Incomplete {
                                    received: a.received,
                                    total: a.total,
                                })
                            }
                            None => {
                                self.counters.calls_timeout.fetch_add(1, Ordering::Relaxed);
                                Err(RpcError::Timeout)
                            }
                        };
                    }
                    attempt += 1;
                    let started = self
                        .pending
                        .lock()
                        .await
                        .get(&id)
                        .map(|p| p.response_started)
                        .unwrap_or(false);
                    if !started {
                        self.counters.retransmits.fetch_add(1, Ordering::Relaxed);
                        let _ = self.egress.send(AuxChannel::Request, &payload).await;
                    }
                }
            }
        }
    }

    /// Remove one pending entry and keep `pending_now` honest. Every removal
    /// outside `dispatch_response` goes through here.
    async fn remove_pending(&self, id: u32) -> Option<PendingCall> {
        let removed = self.pending.lock().await.remove(&id);
        if removed.is_some() {
            self.counters.pending_now.fetch_sub(1, Ordering::Relaxed);
        }
        removed
    }

    /// Route a decoded response fragment to its pending caller, completing the
    /// call as soon as RaptorQ can rebuild the body from what has arrived.
    ///
    /// Called by the response reader task when it decodes a Response payload
    /// off the IPC socket. A fragment whose id matches no pending entry is a
    /// no-op: the caller already gave up (timeout) or the proxy was reset,
    /// both expected on a reordering lane.
    ///
    /// Completion is on sufficiency, not on `total`: a response carries `k`
    /// systematic plus [`aux_rpc::RPC_REPAIR_SYMBOLS`] repair symbols, and
    /// waiting for every index is exactly the geometric loss decay the repair
    /// symbols exist to remove.
    pub async fn dispatch_response(&self, response: &aux_rpc::RpcResponse<'_>) {
        self.counters
            .fragments_received
            .fetch_add(1, Ordering::Relaxed);
        let mut pending = self.pending.lock().await;
        // Taken out of the map for the duration: a fragment that does not
        // complete the call is put back untouched.
        let Some(mut call) = pending.remove(&response.id) else {
            return;
        };

        // Ahead of everything, and deliberately ahead of `response_started`:
        // another drone answering this id says nothing about whether ours got
        // the request, so letting it stop the resend loop guarantees a timeout.
        if call
            .expected_sender
            .as_deref()
            .is_some_and(|expected| response.sender != expected.as_bytes())
        {
            self.counters
                .fragments_wrong_sender
                .fetch_add(1, Ordering::Relaxed);
            pending.insert(response.id, call);
            return;
        }

        // A fragment bearing this id from our drone proves the Request
        // datagram landed, so the resend loop must stop even if this
        // particular fragment turns out to be unusable.
        call.response_started = true;

        if call.assembly.is_none() {
            let bounded =
                response.total > 0 && response.total as usize <= aux_rpc::MAX_RESPONSE_FRAGMENTS;
            let seeded = bounded
                .then(|| aux_rpc::ResponseDecoder::new(response.oti))
                .flatten();
            let Some(decoder) = seeded else {
                // A fragment count or a transfer length we cannot bound, both
                // numbers the radio handed us. Complete the caller with an
                // honest gateway error rather than sizing a decoder from them.
                self.counters
                    .responses_out_of_range
                    .fetch_add(1, Ordering::Relaxed);
                self.counters.pending_now.fetch_sub(1, Ordering::Relaxed);
                let _ = call.sender.send(RpcResponseOwned {
                    status: 502,
                    body: b"relay response geometry out of range".to_vec(),
                });
                return;
            };
            call.assembly = Some(Assembly {
                status: response.status,
                total: response.total,
                received: 0,
                accepted: 0,
                decoder,
            });
        }

        let index = response.index as usize;
        // `total` is capped at MAX_RESPONSE_FRAGMENTS (64), so every in-range
        // index owns a distinct bit; the mask exists only to keep the shift
        // defined for an out-of-range one, which `fresh` then rejects.
        let bit = 1u64 << (index % 64);
        let assembly = call.assembly.as_mut().expect("seeded above");
        // A stale fragment from a recycled id must not reach the decoder, an
        // out-of-range index must not be accepted, and the lane may re-deliver
        // so a duplicate must not be counted or decoded twice.
        let fresh = assembly.total == response.total
            && index < assembly.total as usize
            && assembly.accepted & bit == 0;
        let outcome = fresh.then(|| assembly.decoder.push(response.index, response.body));
        if matches!(outcome, Some(aux_rpc::FragmentOutcome::Pending)) {
            assembly.accepted |= bit;
            assembly.received += 1;
        }
        let status = assembly.status;

        match outcome {
            None => {
                self.counters
                    .fragments_duplicate
                    .fetch_add(1, Ordering::Relaxed);
                pending.insert(response.id, call);
            }
            Some(aux_rpc::FragmentOutcome::BadSymbol) => {
                self.counters
                    .fragments_bad_symbol
                    .fetch_add(1, Ordering::Relaxed);
                pending.insert(response.id, call);
            }
            Some(aux_rpc::FragmentOutcome::Pending) => {
                pending.insert(response.id, call);
            }
            Some(aux_rpc::FragmentOutcome::Complete(body)) => {
                self.counters.pending_now.fetch_sub(1, Ordering::Relaxed);
                let _ = call.sender.send(RpcResponseOwned { status, body });
            }
        }
    }

    /// A synchronous read of the lane's counters, for the status route.
    pub fn stats(&self) -> RpcProxyStats {
        let c = &self.counters;
        RpcProxyStats {
            calls_started: c.calls_started.load(Ordering::Relaxed),
            calls_ok: c.calls_ok.load(Ordering::Relaxed),
            calls_timeout: c.calls_timeout.load(Ordering::Relaxed),
            calls_incomplete: c.calls_incomplete.load(Ordering::Relaxed),
            calls_send_error: c.calls_send_error.load(Ordering::Relaxed),
            calls_channel_closed: c.calls_channel_closed.load(Ordering::Relaxed),
            calls_busy: c.calls_busy.load(Ordering::Relaxed),
            retransmits: c.retransmits.load(Ordering::Relaxed),
            fragments_received: c.fragments_received.load(Ordering::Relaxed),
            fragments_duplicate: c.fragments_duplicate.load(Ordering::Relaxed),
            fragments_wrong_sender: c.fragments_wrong_sender.load(Ordering::Relaxed),
            fragments_bad_symbol: c.fragments_bad_symbol.load(Ordering::Relaxed),
            responses_out_of_range: c.responses_out_of_range.load(Ordering::Relaxed),
            pending_now: c.pending_now.load(Ordering::Relaxed),
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

impl PendingGuard {
    /// Called on any path that already removed the entry itself.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let pending = Arc::clone(&self.pending);
        let counters = Arc::clone(&self.counters);
        let id = self.id;
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if pending.lock().await.remove(&id).is_some() {
                    counters.pending_now.fetch_sub(1, Ordering::Relaxed);
                }
            });
        }
    }
}

/// The listener loop: bind, accept connections, read length-prefixed Response
/// payloads, dispatch to pending callers.
async fn run_response_listener(proxy: AuxRpcProxy, sock_path: PathBuf, cancel: Arc<Notify>) {
    // Remove a stale socket file from a previous process. A leftover file from
    // an unclean shutdown prevents the new bind from succeeding, which would
    // silently disable the response dispatch path for the whole process
    // lifetime. Removing first is the same pattern the mavlink state socket uses.
    let _ = std::fs::remove_file(&sock_path);

    let listener = match UnixListener::bind(&sock_path) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, path = %sock_path.display(), "aux_rpc_response_listener_bind_failed");
            return;
        }
    };
    tracing::info!(path = %sock_path.display(), "aux_rpc_response_listener_listening");

    loop {
        tokio::select! {
            biased;
            _ = cancel.notified() => break,
            accept_result = listener.accept() => {
                match accept_result {
                    Ok((stream, _)) => {
                        let proxy = proxy.clone();
                        tokio::spawn(handle_one_connection(stream, proxy));
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "aux_rpc_response_listener_accept_failed");
                    }
                }
            }
        }
    }
    // Shutdown: complete every waiter now rather than leaving each one to burn
    // its full 10-second bound against a consumer that is already gone.
    proxy.reset().await;
    tracing::info!("aux_rpc_response_listener_stopped");
    let _ = std::fs::remove_file(&sock_path);
}

/// Read length-prefixed Response payloads off one accepted connection until EOF.
///
/// [`AuxRpcResponseIngest::send`] writes exactly one fragment per
/// length-prefixed frame, so each read is one whole payload. It is decoded on
/// its own rather than accumulated: a fragmented response is 25 frames on the
/// wire, and one undecodable frame appended to a shared buffer would poison
/// every frame after it for the life of the connection.
async fn handle_one_connection(mut stream: UnixStream, proxy: AuxRpcProxy) {
    loop {
        match read_one_frame(&mut stream).await {
            Ok(payload) => match aux_rpc::decode_response(&payload) {
                Ok(response) => proxy.dispatch_response(&response).await,
                Err(e) => {
                    tracing::debug!(error = ?e, len = payload.len(), "aux_rpc_response_undecodable");
                }
            },
            Err(e) => {
                if e.kind() != std::io::ErrorKind::UnexpectedEof {
                    tracing::debug!(error = %e, "aux_rpc_response_reader_read_failed");
                }
                break;
            }
        }
    }
}

/// Read one length-prefixed frame off the stream. Returns the payload bytes.
async fn read_one_frame(stream: &mut UnixStream) -> std::io::Result<Vec<u8>> {
    let mut header = [0u8; HEADER_SIZE];
    stream.read_exact(&mut header).await?;
    let len = u32::from_be_bytes(header) as usize;
    if len > aux_mux::AUX_MAX_PAYLOAD {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "response frame exceeds the aux payload maximum",
        ));
    }
    let mut payload = vec![0u8; len];
    if len > 0 {
        stream.read_exact(&mut payload).await?;
    }
    Ok(payload)
}

/// Writer side: CONNECTS to the Response IPC socket and writes each Response
/// payload as a length-prefixed frame. Used by the aux consumer's process to
/// forward Response datagrams to the proxy's process.
///
/// Mirrors `MavlinkIngest` exactly: lazy connect, bounded retry, best-effort
/// write (a full pipe drops rather than blocks the consumer's read loop).
#[derive(Clone)]
pub struct AuxRpcResponseIngest {
    sock_path: PathBuf,
    conn: Arc<Mutex<Option<UnixStream>>>,
}

impl AuxRpcResponseIngest {
    /// An ingest writing to the given Unix socket path.
    pub fn new(sock_path: impl Into<PathBuf>) -> Self {
        Self {
            sock_path: sock_path.into(),
            conn: Arc::new(Mutex::new(None)),
        }
    }

    /// Forward one Response payload as a length-prefixed frame.
    ///
    /// Best-effort: a failed or slow write drops the frame and drops the
    /// connection so the next call re-connects.
    ///
    /// Both the connect and the write are BOUNDED, and that is the whole
    /// point. The caller awaits this inside the aux consumer's single
    /// `recv_from` loop, so an unbounded write to a proxy that has stopped
    /// reading its socket stalls that slot's entire read loop — not just RPC,
    /// but that drone's MAVLink ingest, its status frames and its peer cache.
    /// The doc used to claim this never blocks while `write_all` on a full
    /// socket buffer did exactly that.
    pub async fn send(&self, payload: &[u8]) {
        // Length-prefix the payload: 4-byte BE length + body, matching the
        // frame module's framing shared across all the agent's IPC.
        let len = payload.len() as u32;
        let mut frame = Vec::with_capacity(HEADER_SIZE + payload.len());
        frame.extend_from_slice(&len.to_be_bytes());
        frame.extend_from_slice(payload);
        let mut guard = self.conn.lock().await;
        match guard.as_mut() {
            Some(stream) => {
                let wrote = tokio::time::timeout(IPC_WRITE_TIMEOUT, stream.write_all(&frame)).await;
                // A timeout and an error are treated alike: drop the
                // connection so the next call reconnects. A peer that stopped
                // reading is not going to start because we waited longer, and
                // the caller's read loop is what pays for the wait.
                if !matches!(wrote, Ok(Ok(()))) {
                    if wrote.is_err() {
                        tracing::warn!(
                            timeout_ms = IPC_WRITE_TIMEOUT.as_millis() as u64,
                            "aux_rpc_ingest_write_timed_out"
                        );
                    }
                    *guard = None;
                }
            }
            None => {
                match tokio::time::timeout(
                    IPC_CONNECT_TIMEOUT,
                    UnixStream::connect(&self.sock_path),
                )
                .await
                {
                    Ok(Ok(mut stream)) => {
                        let wrote =
                            tokio::time::timeout(IPC_WRITE_TIMEOUT, stream.write_all(&frame)).await;
                        if matches!(wrote, Ok(Ok(()))) {
                            *guard = Some(stream);
                        }
                    }
                    Ok(Err(_)) => {
                        // No listener yet; the proxy will start eventually.
                    }
                    Err(_) => {
                        tracing::warn!(
                            timeout_ms = IPC_CONNECT_TIMEOUT.as_millis() as u64,
                            "aux_rpc_ingest_connect_timed_out"
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A peer that accepts a connection and then never reads.
    ///
    /// This is the shape that stalls a read loop: the socket buffer fills, the
    /// write blocks, and every drone behind that consumer goes quiet.
    async fn deaf_listener(path: &std::path::Path) -> tokio::task::JoinHandle<()> {
        let listener = tokio::net::UnixListener::bind(path).unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            loop {
                match listener.accept().await {
                    // Hold the connection open and never read from it.
                    Ok((s, _)) => held.push(s),
                    Err(_) => return,
                }
            }
        })
    }

    #[tokio::test]
    async fn a_peer_that_stops_reading_cannot_stall_the_caller() {
        // The caller awaits this inside the aux consumer's single recv loop, so
        // an unbounded write to a wedged proxy stalls that slot's whole inbound
        // path — MAVLink ingest, status frames and the peer cache, not just RPC.
        let dir = std::env::temp_dir().join(format!("ados-ingest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("ingest.sock");
        let _listener = deaf_listener(&sock).await;

        let ingest = AuxRpcResponseIngest::new(&sock);
        let big = vec![0u8; 512 * 1024];

        // Write until the buffer fills; every call must return promptly.
        let started = std::time::Instant::now();
        for _ in 0..8 {
            ingest.send(&big).await;
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "sends took {elapsed:?}; a wedged peer is stalling the caller"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_absent_listener_returns_promptly() {
        let ingest =
            AuxRpcResponseIngest::new(std::path::Path::new("/nonexistent/ados/ingest.sock"));
        let started = std::time::Instant::now();
        ingest.send(b"payload").await;
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn the_ipc_bounds_are_far_below_the_call_bound() {
        // These guard a read loop, not a call. A bound anywhere near the RPC
        // timeout would let one wedged proxy silence a drone for seconds.
        assert!(IPC_WRITE_TIMEOUT < RPC_DEFAULT_TIMEOUT / 10);
        assert!(IPC_CONNECT_TIMEOUT < RPC_DEFAULT_TIMEOUT / 10);
    }
    use crate::aux_mux;
    use tokio::net::UdpSocket;
    use tokio::time::Duration;

    /// A no-radio egress that the test can read back from.
    async fn loopback_egress() -> (
        AuxEgress,
        tokio::sync::mpsc::Receiver<(AuxChannel, Vec<u8>)>,
    ) {
        let listener = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sock.connect(("127.0.0.1", port)).await.unwrap();
        let egress = AuxEgress::connected_for_test(sock);

        let (tx, rx) = tokio::sync::mpsc::channel::<(AuxChannel, Vec<u8>)>(8);
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            while let Ok((n, _)) = listener.recv_from(&mut buf).await {
                if let Ok((ch, payload)) = aux_mux::decode(&buf[..n]) {
                    let _ = tx.send((ch, payload.to_vec())).await;
                }
            }
        });
        (egress, rx)
    }

    /// The device id the tests address, and the one their drone answers with.
    const DRONE: &[u8] = b"77735cd38937";

    /// The encoded aux payloads for one response, exactly as a drone emits
    /// them: RaptorQ symbols, each stamped with the sender's device id.
    fn response_payloads(sender: &[u8], id: u32, status: u16, body: &[u8]) -> Vec<Vec<u8>> {
        let split = aux_rpc::split_response(body).unwrap();
        let total = split.symbols.len() as u16;
        split
            .symbols
            .iter()
            .enumerate()
            .map(|(index, symbol)| {
                aux_rpc::encode_response_fragment(
                    sender,
                    id,
                    status,
                    index as u16,
                    total,
                    split.oti,
                    symbol,
                )
                .unwrap()
            })
            .collect()
    }

    /// Decode one encoded fragment and hand it to the proxy, the way the
    /// response listener does.
    async fn deliver(proxy: &AuxRpcProxy, payload: &[u8]) {
        let fragment = aux_rpc::decode_response(payload).unwrap();
        proxy.dispatch_response(&fragment).await;
    }

    /// Deliver every fragment of one response.
    async fn deliver_body(proxy: &AuxRpcProxy, sender: &[u8], id: u32, status: u16, body: &[u8]) {
        for payload in response_payloads(sender, id, status, body) {
            deliver(proxy, &payload).await;
        }
    }

    /// Sniff the id of the single call the proxy currently has in flight.
    async fn pending_id(proxy: &AuxRpcProxy) -> u32 {
        let pending = proxy.pending.lock().await;
        *pending.keys().next().expect("one pending caller")
    }

    #[tokio::test]
    async fn a_call_round_trips_through_egress_and_a_dispatched_response() {
        let (egress, mut sent) = loopback_egress().await;
        let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(500));

        let proxy_for_consumer = proxy.clone();
        let consumer = tokio::spawn(async move {
            let (channel, payload) = sent.recv().await.expect("no request datagram");
            assert_eq!(channel, AuxChannel::Request);
            let request = aux_rpc::decode_request(&payload).unwrap();
            assert_eq!(request.method, RpcMethod::Get);
            assert_eq!(request.target, DRONE);
            assert_eq!(request.path, b"/api/pairing/info");
            assert!(request.body.is_empty());

            let body = br#"{"device_id":"abc"}"#;
            deliver_body(&proxy_for_consumer, DRONE, request.id, 200, body).await;
        });

        let result = proxy
            .call(DRONE, RpcMethod::Get, b"/api/pairing/info", &[])
            .await
            .expect("call must succeed");

        consumer.await.unwrap();
        assert_eq!(result.status, 200);
        assert_eq!(result.body, br#"{"device_id":"abc"}"#);
    }

    #[tokio::test]
    async fn a_multi_fragment_response_completes_on_its_systematic_symbols_alone() {
        let (egress, mut sent) = loopback_egress().await;
        let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(500));

        let body: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
        let expected = body.clone();
        let proxy_call = proxy.clone();
        let call = tokio::spawn(async move {
            proxy_call
                .call(DRONE, RpcMethod::Get, b"/api/services", &[])
                .await
        });

        let (_, payload) = sent.recv().await.expect("request datagram");
        let id = aux_rpc::decode_request(&payload).unwrap().id;
        let fragments = response_payloads(DRONE, id, 200, &body);
        assert_eq!(fragments.len(), 3 + aux_rpc::RPC_REPAIR_SYMBOLS as usize);
        // Only the systematic symbols. The repair symbols are the cushion, not
        // a prerequisite: a caller that waited for all seven would be back to
        // the geometric loss decay they exist to remove.
        for fragment in &fragments[..3] {
            deliver(&proxy, fragment).await;
        }

        let result = call.await.unwrap().expect("call must succeed");
        assert_eq!(result.status, 200);
        assert_eq!(result.body, expected);
    }

    #[tokio::test]
    async fn fragments_delivered_out_of_order_reassemble_identically() {
        let (egress, mut sent) = loopback_egress().await;
        let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(500));

        let body: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
        let expected = body.clone();
        let proxy_call = proxy.clone();
        let call = tokio::spawn(async move {
            proxy_call
                .call(DRONE, RpcMethod::Get, b"/api/services", &[])
                .await
        });

        let (_, payload) = sent.recv().await.expect("request datagram");
        let id = aux_rpc::decode_request(&payload).unwrap().id;
        let mut fragments = response_payloads(DRONE, id, 200, &body);
        fragments.reverse();
        for fragment in &fragments {
            deliver(&proxy, fragment).await;
        }

        let result = call.await.unwrap().expect("call must succeed");
        assert_eq!(result.body, expected);
    }

    #[tokio::test]
    async fn a_thirty_kilobyte_response_completes_after_losing_four_fragments() {
        // The measured `/api/status/full`. All-or-nothing reassembly of its 26
        // fragments at the measured 0.7% per-fragment loss succeeded 17/20 on
        // the bench; the four repair symbols are what remove that decay.
        let (egress, mut sent) = loopback_egress().await;
        let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_secs(2));

        let body: Vec<u8> = (0..29_339u32).map(|i| (i % 251) as u8).collect();
        let expected = body.clone();
        let proxy_call = proxy.clone();
        let call = tokio::spawn(async move {
            proxy_call
                .call(DRONE, RpcMethod::Get, b"/api/status/full", &[])
                .await
        });

        let (_, payload) = sent.recv().await.expect("request datagram");
        let id = aux_rpc::decode_request(&payload).unwrap().id;
        let mut fragments = response_payloads(DRONE, id, 200, &body);
        assert_eq!(fragments.len(), 30, "26 systematic + 4 repair");
        // Reordered by the lane, and four of them never arrive.
        fragments.rotate_left(7);
        fragments.truncate(26);
        for fragment in &fragments {
            deliver(&proxy, fragment).await;
        }

        let result = call.await.unwrap().expect("26 of 30 must be enough");
        assert_eq!(result.body, expected);
        assert_eq!(proxy.stats().calls_ok, 1);
    }

    #[tokio::test]
    async fn a_fragment_from_another_drone_is_dropped_and_counted() {
        // 24 drones share one fleet key, so a request id is only unique per
        // sender. Splicing a second drone's symbols into this call's decoder
        // yields a cleanly-decoded body belonging to neither aircraft —
        // RaptorQ has no per-object checksum that would catch it.
        let (egress, mut sent) = loopback_egress().await;
        let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(300));

        let proxy_call = proxy.clone();
        let call =
            tokio::spawn(
                async move { proxy_call.call(DRONE, RpcMethod::Get, b"/api/x", &[]).await },
            );

        let (_, payload) = sent.recv().await.expect("request datagram");
        let id = aux_rpc::decode_request(&payload).unwrap().id;
        let imposter = response_payloads(b"ados-elsewhere", id, 200, b"not-your-answer");
        for fragment in &imposter {
            deliver(&proxy, fragment).await;
        }

        let err = call.await.unwrap().unwrap_err();
        let stats = proxy.stats();
        assert_eq!(
            err,
            RpcError::Timeout,
            "an imposter must not complete a call"
        );
        assert_eq!(stats.fragments_wrong_sender, imposter.len() as u64);
        assert_eq!(stats.calls_ok, 0);
        assert!(
            stats.retransmits > 0,
            "another drone's answer proves nothing about ours, so the resend \
             loop must keep running"
        );
    }

    #[tokio::test]
    async fn the_addressed_drone_still_completes_the_call_after_an_imposter() {
        // Dropping the wrong sender's fragments must not poison the pending
        // entry: the real answer, arriving second, still has to land.
        let (egress, mut sent) = loopback_egress().await;
        let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_secs(2));

        let proxy_call = proxy.clone();
        let call =
            tokio::spawn(
                async move { proxy_call.call(DRONE, RpcMethod::Get, b"/api/x", &[]).await },
            );

        let (_, payload) = sent.recv().await.expect("request datagram");
        let id = aux_rpc::decode_request(&payload).unwrap().id;
        for fragment in &response_payloads(b"ados-elsewhere", id, 200, b"wrong-drone") {
            deliver(&proxy, fragment).await;
        }
        deliver_body(&proxy, DRONE, id, 200, b"right-drone").await;

        let result = call.await.unwrap().expect("the addressed drone answered");
        assert_eq!(result.body, b"right-drone");
    }

    #[tokio::test]
    async fn a_broadcast_call_accepts_whichever_drone_answers() {
        // An empty target is a broadcast: there is no single sender to hold
        // the fragments to, and gating one would break the diagnostic call
        // that does not know a device id yet.
        let (egress, mut sent) = loopback_egress().await;
        let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(500));

        let proxy_call = proxy.clone();
        let call =
            tokio::spawn(async move { proxy_call.call(&[], RpcMethod::Get, b"/api/x", &[]).await });

        let (_, payload) = sent.recv().await.expect("request datagram");
        let id = aux_rpc::decode_request(&payload).unwrap().id;
        deliver_body(&proxy, b"ados-anyone", id, 200, b"answered").await;

        let result = call.await.unwrap().expect("call must succeed");
        assert_eq!(result.body, b"answered");
        assert_eq!(proxy.stats().fragments_wrong_sender, 0);
    }

    #[tokio::test]
    async fn a_duplicate_fragment_does_not_double_count() {
        let (egress, mut sent) = loopback_egress().await;
        let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(400));

        let body: Vec<u8> = (0..2000u32).map(|i| (i % 251) as u8).collect();
        let expected = body.clone();
        let proxy_call = proxy.clone();
        let call =
            tokio::spawn(
                async move { proxy_call.call(DRONE, RpcMethod::Get, b"/api/x", &[]).await },
            );

        let (_, payload) = sent.recv().await.expect("request datagram");
        let id = aux_rpc::decode_request(&payload).unwrap().id;
        let fragments = response_payloads(DRONE, id, 200, &body);
        // Symbol 0 twice, then 1: a naive counter would report two received
        // with symbol 1 still missing.
        for i in [0usize, 0, 1] {
            deliver(&proxy, &fragments[i]).await;
        }

        let result = call.await.unwrap().expect("call must succeed");
        assert_eq!(result.body, expected);
        assert_eq!(proxy.stats().fragments_duplicate, 1);
    }

    #[tokio::test]
    async fn a_missing_fragment_yields_incomplete_not_timeout() {
        let (egress, mut sent) = loopback_egress().await;
        let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(120));

        let body: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
        let proxy_for_consumer = proxy.clone();
        let consumer = tokio::spawn(async move {
            let (_, payload) = sent.recv().await.expect("request datagram");
            let id = aux_rpc::decode_request(&payload).unwrap().id;
            let fragments = response_payloads(DRONE, id, 200, &body);
            deliver(&proxy_for_consumer, &fragments[0]).await;
        });

        let err = proxy
            .call(DRONE, RpcMethod::Get, b"/api/x", &[])
            .await
            .unwrap_err();
        consumer.await.unwrap();
        assert_eq!(
            err,
            RpcError::Incomplete {
                received: 1,
                total: 7
            }
        );
        assert_eq!(
            err.to_string(),
            "relay response incomplete: 1/7 fragments",
            "the operator must be able to tell a dropped fragment from silence"
        );
    }

    #[tokio::test]
    async fn a_fragment_count_past_the_ceiling_completes_with_502() {
        let (egress, mut sent) = loopback_egress().await;
        let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(400));

        let proxy_for_consumer = proxy.clone();
        let consumer = tokio::spawn(async move {
            let (_, payload) = sent.recv().await.expect("request datagram");
            let id = aux_rpc::decode_request(&payload).unwrap().id;
            proxy_for_consumer
                .dispatch_response(&aux_rpc::RpcResponse {
                    id,
                    sender: DRONE,
                    status: 200,
                    index: 0,
                    total: 65,
                    oti: 2,
                    body: b"aa",
                })
                .await;
        });

        let result = proxy
            .call(DRONE, RpcMethod::Get, b"/api/x", &[])
            .await
            .expect("the caller must be completed, not left hanging");
        consumer.await.unwrap();
        assert_eq!(result.status, 502);
        assert_eq!(result.body, b"relay response geometry out of range");
        assert_eq!(proxy.stats().responses_out_of_range, 1);
    }

    #[tokio::test]
    async fn a_transfer_length_past_the_ceiling_completes_with_502() {
        // `oti` is a number the radio handed us and RaptorQ sizes its matrices
        // from it. A corrupt one must be refused, not allocated for.
        let (egress, mut sent) = loopback_egress().await;
        let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(400));

        let proxy_for_consumer = proxy.clone();
        let consumer = tokio::spawn(async move {
            let (_, payload) = sent.recv().await.expect("request datagram");
            let id = aux_rpc::decode_request(&payload).unwrap().id;
            proxy_for_consumer
                .dispatch_response(&aux_rpc::RpcResponse {
                    id,
                    sender: DRONE,
                    status: 200,
                    index: 0,
                    total: 2,
                    oti: u32::MAX,
                    body: b"aa",
                })
                .await;
        });

        let result = proxy
            .call(DRONE, RpcMethod::Get, b"/api/x", &[])
            .await
            .expect("the caller must be completed, not left hanging");
        consumer.await.unwrap();
        assert_eq!(result.status, 502);
        assert_eq!(proxy.stats().responses_out_of_range, 1);
    }

    #[tokio::test]
    async fn a_symbol_of_the_wrong_size_is_dropped_and_counted() {
        // Radio damage, or two ends on different builds. Feeding it to RaptorQ
        // would corrupt a matrix that assumes every symbol is one size.
        let (egress, mut sent) = loopback_egress().await;
        let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(200));

        let proxy_for_consumer = proxy.clone();
        let consumer = tokio::spawn(async move {
            let (_, payload) = sent.recv().await.expect("request datagram");
            let id = aux_rpc::decode_request(&payload).unwrap().id;
            proxy_for_consumer
                .dispatch_response(&aux_rpc::RpcResponse {
                    id,
                    sender: DRONE,
                    status: 200,
                    index: 0,
                    total: 5,
                    oti: 64,
                    body: b"only-nine",
                })
                .await;
        });

        let err = proxy
            .call(DRONE, RpcMethod::Get, b"/api/x", &[])
            .await
            .unwrap_err();
        consumer.await.unwrap();
        assert_eq!(
            err,
            RpcError::Incomplete {
                received: 0,
                total: 5
            }
        );
        assert_eq!(proxy.stats().fragments_bad_symbol, 1);
    }

    #[tokio::test]
    async fn a_call_times_out_when_no_response_arrives() {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sock.connect(("127.0.0.1", 9u16)).await.unwrap();
        let egress = AuxEgress::connected_for_test(sock);
        let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(80));

        let started = std::time::Instant::now();
        let err = proxy
            .call(&[], RpcMethod::Get, b"/api/status", &[])
            .await
            .unwrap_err();
        assert_eq!(err, RpcError::Timeout);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn a_call_returns_send_error_when_egress_fails() {
        let egress =
            AuxEgress::with_timeout("/nonexistent/aux-cmd.sock", Duration::from_millis(40));
        let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(200));
        let err = proxy
            .call(&[], RpcMethod::Get, b"/api/status", &[])
            .await
            .unwrap_err();
        assert!(
            matches!(err, RpcError::Send(_)),
            "expected Send, got {err:?}"
        );
    }

    #[tokio::test]
    async fn a_call_returns_encode_when_the_path_exceeds_one_aux_frame() {
        let (egress, _sent) = loopback_egress().await;
        let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(200));
        let big_path = vec![b'A'; aux_mux::AUX_MAX_PAYLOAD];
        let err = proxy
            .call(&[], RpcMethod::Get, &big_path, &[])
            .await
            .unwrap_err();
        assert_eq!(err, RpcError::Encode);
    }

    #[tokio::test]
    async fn a_response_for_an_unknown_id_is_a_noop() {
        let (egress, _sent) = loopback_egress().await;
        let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(50));
        for fragment in &response_payloads(DRONE, 9999, 200, b"orphan") {
            deliver(&proxy, fragment).await;
        }
        assert!(proxy.pending.lock().await.is_empty());
    }

    #[tokio::test]
    async fn concurrent_calls_with_distinct_ids_match_their_own_responses() {
        let (egress, mut sent) = loopback_egress().await;
        let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(500));
        let proxy_consumer = proxy.clone();

        let consumer = tokio::spawn(async move {
            for _ in 0..3 {
                let (ch, payload) = sent.recv().await.expect("request datagram");
                assert_eq!(ch, AuxChannel::Request);
                let request = aux_rpc::decode_request(&payload).unwrap();
                let body = format!("{}", request.id).into_bytes();
                deliver_body(&proxy_consumer, DRONE, request.id, 200, &body).await;
            }
        });

        let p1 = tokio::spawn({
            let proxy = proxy.clone();
            async move { proxy.call(&[], RpcMethod::Get, b"/api/a", &[]).await }
        });
        let p2 = tokio::spawn({
            let proxy = proxy.clone();
            async move { proxy.call(&[], RpcMethod::Get, b"/api/b", &[]).await }
        });
        let p3 = tokio::spawn({
            let proxy = proxy.clone();
            async move { proxy.call(&[], RpcMethod::Get, b"/api/c", &[]).await }
        });

        let r1 = p1.await.unwrap().unwrap();
        let r2 = p2.await.unwrap().unwrap();
        let r3 = p3.await.unwrap().unwrap();
        consumer.await.unwrap();

        assert_ne!(r1.body, r2.body);
        assert_ne!(r2.body, r3.body);
        assert_eq!(r1.status, 200);
        assert_eq!(r2.status, 200);
        assert_eq!(r3.status, 200);
    }

    #[tokio::test]
    async fn reset_drains_pending_callers_with_503() {
        let (egress, _sent) = loopback_egress().await;
        let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_secs(60));

        let proxy_call = proxy.clone();
        let call_handle =
            tokio::spawn(async move { proxy_call.call(&[], RpcMethod::Get, b"/api/x", &[]).await });

        tokio::time::sleep(Duration::from_millis(20)).await;
        proxy.reset().await;
        let result = tokio::time::timeout(Duration::from_millis(500), call_handle)
            .await
            .expect("reset must unblock the caller within the bound")
            .unwrap();
        assert_eq!(result.unwrap().status, 503);
    }

    #[tokio::test]
    async fn the_ingest_writes_and_the_listener_dispatches() {
        // End-to-end: proxy LISTENS, ingest CONNECTS, a Response written by
        // the ingest arrives at the listener and is dispatched to the pending
        // caller. The call's Request goes into a loopback void (no consumer
        // reading it), but the Response arrives via the IPC seam.
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("aux-rpc-responses.sock");

        // A loopback egress whose Request datagrams nobody reads — the call
        // succeeds on the Response side, not the Request side.
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sock.connect(("127.0.0.1", 9u16)).await.unwrap();
        let egress = AuxEgress::connected_for_test(sock);
        let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(500));

        // Spawn the listener.
        let cancel = Arc::new(Notify::new());
        let listener_handle = proxy.spawn_response_listener(sock_path.clone(), cancel.clone());

        // Give the listener time to bind.
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Start a call — it will send a Request into the void and wait.
        let proxy_call = proxy.clone();
        let call_handle = tokio::spawn(async move {
            proxy_call
                .call(&[], RpcMethod::Get, b"/api/pairing/info", &[])
                .await
        });

        // Give the call time to register its pending entry.
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Write a matching Response via the ingest. The call's request id was
        // assigned by the proxy (starts at 1), so we sniff it from the
        // pending map.
        let id = pending_id(&proxy).await;

        let ingest = AuxRpcResponseIngest::new(&sock_path);
        for payload in response_payloads(DRONE, id, 200, br#"{"ok":true}"#) {
            ingest.send(&payload).await;
        }

        let result = tokio::time::timeout(Duration::from_secs(2), call_handle)
            .await
            .expect("the call must complete within the bound")
            .unwrap();
        assert_eq!(result.unwrap().status, 200);

        cancel.notify_waiters();
        let _ = listener_handle.await;
    }

    #[tokio::test]
    async fn an_undecodable_frame_does_not_poison_the_fragments_after_it() {
        // A 25-fragment response is 25 IPC frames. One damaged frame must cost
        // exactly that fragment, not every fragment behind it.
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("aux-rpc-responses.sock");

        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sock.connect(("127.0.0.1", 9u16)).await.unwrap();
        let egress = AuxEgress::connected_for_test(sock);
        let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_secs(2));

        let cancel = Arc::new(Notify::new());
        let listener_handle = proxy.spawn_response_listener(sock_path.clone(), cancel.clone());
        tokio::time::sleep(Duration::from_millis(20)).await;

        let proxy_call = proxy.clone();
        let call_handle =
            tokio::spawn(async move { proxy_call.call(&[], RpcMethod::Get, b"/api/x", &[]).await });
        tokio::time::sleep(Duration::from_millis(20)).await;

        let id = pending_id(&proxy).await;

        let body: Vec<u8> = (0..2000u32).map(|i| (i % 251) as u8).collect();
        let fragments = response_payloads(DRONE, id, 200, &body);
        let ingest = AuxRpcResponseIngest::new(&sock_path);
        ingest.send(&fragments[0]).await;
        ingest.send(b"not-a-fragment").await;
        ingest.send(&fragments[1]).await;

        let result = tokio::time::timeout(Duration::from_secs(3), call_handle)
            .await
            .expect("the call must complete within the bound")
            .unwrap();
        assert_eq!(result.unwrap().body, body);

        cancel.notify_waiters();
        let _ = listener_handle.await;
    }

    #[tokio::test]
    async fn a_lost_request_datagram_is_retransmitted_until_the_drone_answers() {
        let (egress, mut sent) = loopback_egress().await;
        let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_secs(3));

        let proxy_for_consumer = proxy.clone();
        let consumer = tokio::spawn(async move {
            // The half-duplex radio swallowed the first two attempts while the
            // drone was mid video burst. The third is the one that lands.
            let (_, first) = sent.recv().await.expect("first attempt");
            let (_, second) = sent.recv().await.expect("first retransmit");
            let (_, third) = sent.recv().await.expect("second retransmit");
            assert_eq!(first, second, "a retransmit must be the identical datagram");
            assert_eq!(second, third);
            let request = aux_rpc::decode_request(&third).unwrap();
            deliver_body(
                &proxy_for_consumer,
                DRONE,
                request.id,
                200,
                b"late-but-answered",
            )
            .await;
        });

        let result = proxy
            .call(DRONE, RpcMethod::Get, b"/api/version", &[])
            .await
            .expect("a retransmitted request must still complete");
        consumer.await.unwrap();
        assert_eq!(result.body, b"late-but-answered");
        assert!(
            proxy.stats().retransmits >= 2,
            "one datagram on a 20-40% loss lane is a coin flip; it must be resent"
        );
    }

    #[tokio::test]
    async fn a_response_that_has_started_arriving_stops_the_retransmits() {
        let (egress, mut sent) = loopback_egress().await;
        let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(600));

        let body: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
        let proxy_for_consumer = proxy.clone();
        let consumer = tokio::spawn(async move {
            let (_, payload) = sent.recv().await.expect("request datagram");
            let id = aux_rpc::decode_request(&payload).unwrap().id;
            // One symbol of seven lands; the rest never do. The call still
            // fails Incomplete, but the request demonstrably got through, so
            // resending it would only duplicate the drone's work.
            let fragments = response_payloads(DRONE, id, 200, &body);
            deliver(&proxy_for_consumer, &fragments[0]).await;
            tokio::time::sleep(Duration::from_millis(450)).await;
            sent.try_recv().is_ok()
        });

        let err = proxy
            .call(DRONE, RpcMethod::Get, b"/api/x", &[])
            .await
            .unwrap_err();
        let resent = consumer.await.unwrap();
        assert_eq!(
            err,
            RpcError::Incomplete {
                received: 1,
                total: 7
            }
        );
        assert!(!resent, "no second Request datagram may leave the ground");
        assert_eq!(proxy.stats().retransmits, 0);
    }

    #[tokio::test]
    async fn a_saturated_proxy_sheds_load_instead_of_queueing_radio_work() {
        let (egress, _sent) = loopback_egress().await;
        let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_millis(200));
        {
            let mut pending = proxy.pending.lock().await;
            for id in 0..MAX_PENDING_CALLS as u32 {
                let (tx, _rx) = oneshot::channel::<RpcResponseOwned>();
                pending.insert(
                    id,
                    PendingCall {
                        sender: tx,
                        expected_sender: None,
                        assembly: None,
                        response_started: false,
                    },
                );
            }
        }

        let err = proxy
            .call(&[], RpcMethod::Get, b"/api/x", &[])
            .await
            .unwrap_err();
        assert_eq!(err, RpcError::Busy);
        assert_eq!(err.to_string(), "relay proxy has too many calls in flight");
        assert_eq!(proxy.stats().calls_busy, 1);
    }

    #[tokio::test]
    async fn a_cancelled_call_does_not_leak_its_pending_entry() {
        // An axum handler whose client disconnects drops the call future
        // mid-flight. Post-await cleanup never runs on that path.
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sock.connect(("127.0.0.1", 9u16)).await.unwrap();
        let egress = AuxEgress::connected_for_test(sock);
        let proxy = AuxRpcProxy::with_timeout(egress, Duration::from_secs(60));

        let proxy_call = proxy.clone();
        let handle =
            tokio::spawn(async move { proxy_call.call(&[], RpcMethod::Get, b"/api/x", &[]).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(proxy.pending.lock().await.len(), 1);

        handle.abort();
        // The guard schedules its removal rather than taking the async mutex
        // inline, so give the spawned cleanup a turn.
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(
            proxy.pending.lock().await.is_empty(),
            "a dropped call future must not leak its sender and assembly forever"
        );
        assert_eq!(proxy.stats().pending_now, 0);
    }

    #[tokio::test]
    async fn the_request_id_seed_is_not_a_fixed_one() {
        // Two proxies standing in for two process starts: if both began at 1, a
        // stale in-flight response from before a restart would silently
        // complete a different new caller.
        let (egress_a, _a) = loopback_egress().await;
        let (egress_b, _b) = loopback_egress().await;
        let a = AuxRpcProxy::with_timeout(egress_a, Duration::from_millis(50));
        let b = AuxRpcProxy::with_timeout(egress_b, Duration::from_millis(50));
        assert_ne!(a.next_id.load(Ordering::Relaxed), 1);
        assert_ne!(b.next_id.load(Ordering::Relaxed), 1);
    }
}
