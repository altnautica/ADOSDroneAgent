//! Shared plumbing for the direct-GCS proxy edges: bind retry with a listening
//! flag, accept-error backoff, the per-client MAVLink framer, the UDP verdict
//! cache and the UDP fan-out target set.

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use ados_protocol::pairing_posture::Access;
use ados_protocol::shutdown::Shutdown;

use crate::connection::{
    extract_frames, ClientOrigin, FcConnection, FC_MAX_SINGLE_WRITE, RECONNECT_INTERVAL,
};

/// Pause after a failed `accept`. An accept error (EMFILE, ENFILE, ENOBUFS)
/// leaves the listener ready, so without a pause the loop spins a core.
pub(super) const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(200);

/// At most one `proxy_accept_failed` line per this window per listener.
const ACCEPT_ERROR_LOG_EVERY: Duration = Duration::from_secs(10);

/// How often a live TCP or WebSocket session re-checks its admission against
/// the current pairing posture.
pub(super) const AUTH_RECHECK: Duration = Duration::from_secs(2);

/// Largest WebSocket message or frame accepted. The FC path takes at most
/// [`FC_MAX_SINGLE_WRITE`] per write, so anything far larger is buffered only
/// to be thrown away.
pub(super) const WS_MAX_MESSAGE: usize = 64 * 1024;

/// A refused UDP peer is logged at most once per this window.
const UDP_REFUSAL_LOG_EVERY: Duration = Duration::from_secs(60);

/// How often the UDP fan-out evicts quiet peers.
pub(super) const UDP_EVICT_EVERY: Duration = Duration::from_secs(1);

/// Upper bound on a client's reassembly buffer. A client frame is at most 280
/// bytes, so this only trips on a stream that is not MAVLink at all.
const CLIENT_BUF_CAP: usize = 64 * 1024;

/// Whether one proxy edge currently holds its listening socket. Set on bind,
/// cleared when the proxy task returns.
#[derive(Debug, Clone, Default)]
pub struct ListenFlag(Arc<AtomicBool>);

impl ListenFlag {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the edge is bound right now.
    pub fn get(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    pub(super) fn set(&self, listening: bool) {
        self.0.store(listening, Ordering::Relaxed);
    }
}

/// Clears a [`ListenFlag`] when the proxy task returns or is aborted.
pub(super) struct ListeningGuard(ListenFlag);

impl ListeningGuard {
    pub(super) fn new(flag: &ListenFlag) -> Self {
        flag.set(true);
        Self(flag.clone())
    }
}

impl Drop for ListeningGuard {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

/// Bind with `bind`, retrying every [`RECONNECT_INTERVAL`] with no cap until it
/// succeeds or `cancel` fires (`None`). `on_error` logs each failure.
pub(super) async fn bind_until<T, F, Fut>(
    cancel: &Shutdown,
    mut bind: F,
    on_error: impl Fn(&std::io::Error),
) -> Option<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = std::io::Result<T>>,
{
    loop {
        match bind().await {
            Ok(bound) => return Some(bound),
            Err(e) => on_error(&e),
        }
        tokio::select! {
            _ = tokio::time::sleep(RECONNECT_INTERVAL) => {}
            _ = cancel.wait() => return None,
        }
    }
}

/// Rate-limited logging plus backoff for failed `accept` calls.
#[derive(Debug, Default)]
pub(super) struct AcceptErrors {
    last_log: Option<Instant>,
    suppressed: u64,
}

impl AcceptErrors {
    /// Record one failed accept on `port`: log it unless one was logged in the
    /// last [`ACCEPT_ERROR_LOG_EVERY`], then wait [`ACCEPT_ERROR_BACKOFF`].
    /// Returns whether this call logged.
    pub(super) async fn on_error(&mut self, port: u16, e: &std::io::Error) -> bool {
        let now = Instant::now();
        let log = self
            .last_log
            .is_none_or(|at| now.saturating_duration_since(at) >= ACCEPT_ERROR_LOG_EVERY);
        if log {
            tracing::warn!(port, error = %e, suppressed = self.suppressed, "proxy_accept_failed");
            self.last_log = Some(now);
            self.suppressed = 0;
        } else {
            self.suppressed += 1;
        }
        tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
        log
    }
}

/// Reassembles a client's byte stream into whole MAVLink frames so a frame is
/// never split across two writes to the flight controller.
#[derive(Debug, Default)]
pub(super) struct ClientFramer {
    buf: Vec<u8>,
}

impl ClientFramer {
    /// Feed `data`; returns the complete frames it finished, concatenated into
    /// batches of at most [`FC_MAX_SINGLE_WRITE`] bytes. A partial trailing
    /// frame stays buffered for the next call.
    pub(super) fn push(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        self.buf.extend_from_slice(data);
        let frames = extract_frames(&mut self.buf);
        if self.buf.len() > CLIENT_BUF_CAP {
            self.buf.clear();
        }
        let mut batches = Vec::new();
        let mut batch: Vec<u8> = Vec::new();
        for frame in frames {
            if !batch.is_empty() && batch.len() + frame.len() > FC_MAX_SINGLE_WRITE {
                batches.push(std::mem::take(&mut batch));
            }
            batch.extend_from_slice(&frame);
        }
        if !batch.is_empty() {
            batches.push(batch);
        }
        batches
    }
}

/// Forward a client's bytes toward the FC: whole frames only, batched, for a
/// MAVLink FC; raw for an MSP FC, which does not speak MAVLink.
pub(super) async fn forward_client_bytes(
    fc: &FcConnection,
    framer: &mut ClientFramer,
    data: &[u8],
    origin: ClientOrigin,
) {
    if fc.fc_variant().await.is_some() {
        let _ = fc.send_client_bytes(data, origin, None).await;
        return;
    }
    for batch in framer.push(data) {
        let _ = fc.send_client_bytes(&batch, origin, None).await;
    }
}

/// One UDP peer's cached admission verdict.
#[derive(Debug)]
struct Verdict {
    origin: Option<ClientOrigin>,
    access: Access,
    decided_at: Instant,
    used_at: Instant,
    warned_at: Option<Instant>,
    suppressed: u64,
}

/// What the UDP loop should do with one datagram.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct UdpAdmission {
    /// `Some(origin)` to serve the datagram, `None` to drop it.
    pub(super) origin: Option<ClientOrigin>,
    /// `Some(suppressed)` when an unauthorized-peer warning is due now, with
    /// the count of datagrams since the last warning.
    pub(super) warn: Option<u64>,
}

/// Per-peer admission cache for the UDP proxy. The verdict for a source
/// address is reused for `ttl`, so the route lookup and the pairing read run
/// once per peer per window rather than per datagram, and an unauthorized
/// peer is logged once per [`UDP_REFUSAL_LOG_EVERY`].
pub(super) struct UdpVerdicts {
    entries: HashMap<SocketAddr, Verdict>,
    ttl: Duration,
    cap: usize,
}

impl UdpVerdicts {
    pub(super) fn new(ttl: Duration, cap: usize) -> Self {
        Self {
            entries: HashMap::with_capacity(cap),
            ttl,
            cap,
        }
    }

    /// The admission for a datagram from `peer` at `now`. `decide` runs only
    /// when the peer has no fresh verdict.
    pub(super) fn admit(
        &mut self,
        peer: SocketAddr,
        now: Instant,
        decide: impl FnOnce() -> (Option<ClientOrigin>, Access),
    ) -> UdpAdmission {
        let stale = self
            .entries
            .get(&peer)
            .is_none_or(|v| now.saturating_duration_since(v.decided_at) >= self.ttl);
        if stale {
            let (origin, access) = decide();
            if !self.entries.contains_key(&peer) && self.entries.len() >= self.cap {
                if let Some(oldest) = self
                    .entries
                    .iter()
                    .min_by_key(|(_, v)| v.used_at)
                    .map(|(a, _)| *a)
                {
                    self.entries.remove(&oldest);
                }
            }
            let entry = self.entries.entry(peer).or_insert(Verdict {
                origin,
                access,
                decided_at: now,
                used_at: now,
                warned_at: None,
                suppressed: 0,
            });
            entry.origin = origin;
            entry.access = access;
            entry.decided_at = now;
        }
        let Some(entry) = self.entries.get_mut(&peer) else {
            return UdpAdmission {
                origin: None,
                warn: None,
            };
        };
        entry.used_at = now;
        let mut warn = None;
        if entry.access != Access::Accept {
            let due = entry
                .warned_at
                .is_none_or(|at| now.saturating_duration_since(at) >= UDP_REFUSAL_LOG_EVERY);
            if due {
                warn = Some(entry.suppressed);
                entry.warned_at = Some(now);
                entry.suppressed = 0;
            } else {
                entry.suppressed += 1;
            }
        }
        UdpAdmission {
            origin: entry.origin,
            warn,
        }
    }
}

/// The learned UDP peers plus a generation that moves whenever the set
/// changes, so the fan-out rebuilds its target list only on a change.
#[derive(Default)]
pub(super) struct UdpPeers {
    seen: StdMutex<HashMap<SocketAddr, Instant>>,
    generation: AtomicU64,
}

impl UdpPeers {
    /// Record a datagram from `peer`; a new peer moves the generation (the
    /// cap can only evict when a new peer arrived).
    pub(super) fn touch(&self, peer: SocketAddr, now: Instant, cap: usize) {
        let mut seen = self.seen.lock().unwrap_or_else(|p| p.into_inner());
        if seen.insert(peer, now).is_none() {
            super::cap_peers(&mut seen, cap);
            self.generation.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Evict quiet peers; an eviction moves the generation.
    pub(super) fn evict(&self, now: Instant) {
        let mut seen = self.seen.lock().unwrap_or_else(|p| p.into_inner());
        let before = seen.len();
        super::evict_stale_peers(&mut seen, now);
        if seen.len() != before {
            self.generation.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(super) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Refill `targets` with the current peers.
    pub(super) fn fill(&self, targets: &mut Vec<SocketAddr>) {
        let seen = self.seen.lock().unwrap_or_else(|p| p.into_inner());
        targets.clear();
        targets.extend(seen.keys().copied());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn heartbeat() -> Vec<u8> {
        use ados_protocol::mavlink::ardupilotmega::{
            MavAutopilot, MavMessage, MavModeFlag, MavState, MavType, HEARTBEAT_DATA,
        };
        let msg = MavMessage::HEARTBEAT(HEARTBEAT_DATA {
            custom_mode: 0,
            mavtype: MavType::MAV_TYPE_GCS,
            autopilot: MavAutopilot::MAV_AUTOPILOT_INVALID,
            base_mode: MavModeFlag::empty(),
            system_status: MavState::MAV_STATE_ACTIVE,
            mavlink_version: 3,
        });
        ados_protocol::mavlink::serialize_v2(
            ados_protocol::mavlink::MavHeader {
                system_id: 255,
                component_id: 190,
                sequence: 0,
            },
            &msg,
        )
        .unwrap()
    }

    #[test]
    fn a_frame_split_across_reads_is_forwarded_whole_once() {
        let frame = heartbeat();
        let mut framer = ClientFramer::default();
        assert!(framer.push(&frame[..7]).is_empty(), "half a frame waits");
        assert_eq!(framer.push(&frame[7..]), vec![frame.clone()]);
    }

    #[test]
    fn many_frames_batch_under_the_write_cap() {
        let frame = heartbeat();
        let mut burst = Vec::new();
        for _ in 0..400 {
            burst.extend_from_slice(&frame);
        }
        let batches = ClientFramer::default().push(&burst);
        assert!(batches.len() > 1);
        assert!(batches.iter().all(|b| b.len() <= FC_MAX_SINGLE_WRITE));
        assert_eq!(batches.iter().map(Vec::len).sum::<usize>(), burst.len());
    }

    #[tokio::test(start_paused = true)]
    async fn accept_errors_back_off_and_log_once_per_window() {
        let mut errors = AcceptErrors::default();
        let e = std::io::Error::other("emfile");
        let start = tokio::time::Instant::now();
        let mut logged = 0;
        for _ in 0..5 {
            if errors.on_error(5760, &e).await {
                logged += 1;
            }
        }
        assert_eq!(logged, 1, "one line per window, the rest counted");
        assert!(
            start.elapsed() >= ACCEPT_ERROR_BACKOFF * 5,
            "each error yields"
        );
    }

    #[test]
    fn udp_refused_peer_one_warn() {
        let mut cache = UdpVerdicts::new(Duration::from_secs(2), 64);
        let peer: SocketAddr = "192.168.1.50:14550".parse().unwrap();
        let t0 = Instant::now();
        let mut lookups = 0;
        let mut warns = 0;
        for i in 0..100u64 {
            let now = t0 + Duration::from_millis(i);
            let got = cache.admit(peer, now, || {
                lookups += 1;
                (None, Access::Unauthorized)
            });
            assert_eq!(got.origin, None);
            if got.warn.is_some() {
                warns += 1;
            }
        }
        assert_eq!(warns, 1);
        assert_eq!(lookups, 1, "one route lookup per TTL");
        // After the TTL the verdict is re-decided; after the log window the
        // warning carries the suppressed count.
        let later = t0 + UDP_REFUSAL_LOG_EVERY;
        let got = cache.admit(peer, later, || (None, Access::Unauthorized));
        assert_eq!(got.warn, Some(99));
    }

    #[test]
    fn the_verdict_cache_is_bounded() {
        let mut cache = UdpVerdicts::new(Duration::from_secs(2), 4);
        let now = Instant::now();
        for p in 0..10u16 {
            let peer: SocketAddr = format!("192.168.1.50:{}", 1000 + p).parse().unwrap();
            cache.admit(peer, now + Duration::from_millis(u64::from(p)), || {
                (Some(ClientOrigin::Trusted), Access::Accept)
            });
        }
        assert_eq!(cache.entries.len(), 4);
    }

    #[test]
    fn the_peer_generation_moves_only_on_a_change() {
        let peers = UdpPeers::default();
        let a: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let now = Instant::now();
        peers.touch(a, now, 64);
        let g = peers.generation();
        peers.touch(a, now, 64);
        assert_eq!(peers.generation(), g, "a refresh is not a change");
        peers.evict(now + super::super::UDP_PEER_TTL + Duration::from_secs(1));
        assert_ne!(peers.generation(), g, "an eviction is");
        let mut targets = vec![a];
        peers.fill(&mut targets);
        assert!(targets.is_empty());
    }
}
