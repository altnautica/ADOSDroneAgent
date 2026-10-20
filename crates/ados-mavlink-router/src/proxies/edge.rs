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

use crate::connection::{ClientFramer, ClientOrigin, FcConnection, RECONNECT_INTERVAL};

/// Pause after a failed `accept`. An accept error (EMFILE, ENFILE, ENOBUFS)
/// leaves the listener ready, so without a pause the loop spins a core.
pub(super) const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(200);

/// At most one `proxy_accept_failed` line per this window per listener.
const ACCEPT_ERROR_LOG_EVERY: Duration = Duration::from_secs(10);

/// How often a live TCP or WebSocket session re-checks its admission against
/// the current pairing posture.
pub(super) const AUTH_RECHECK: Duration = Duration::from_secs(2);

/// Largest WebSocket message or frame accepted. The FC path takes at most
/// [`FC_MAX_SINGLE_WRITE`](crate::connection::FC_MAX_SINGLE_WRITE) per write,
/// so anything far larger is buffered only to be thrown away.
pub(super) const WS_MAX_MESSAGE: usize = 64 * 1024;

/// A refused UDP peer is logged at most once per this window.
const UDP_REFUSAL_LOG_EVERY: Duration = Duration::from_secs(60);

/// How often the UDP fan-out evicts quiet peers.
pub(super) const UDP_EVICT_EVERY: Duration = Duration::from_secs(1);

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

/// Forward a client's bytes toward the FC: whole frames only, batched, for a
/// MAVLink FC; raw for an MSP FC, which does not speak MAVLink. With
/// route-aware forwarding on, `ep` names the client: each batch is learned
/// and handed to the other clients.
pub(super) async fn forward_client_bytes(
    fc: &FcConnection,
    framer: &mut ClientFramer,
    data: &[u8],
    origin: ClientOrigin,
    ep: Option<&crate::routing::Endpoint>,
) {
    if fc.fc_variant().await.is_some() {
        let _ = fc.send_client_bytes(data, origin, None).await;
        return;
    }
    for batch in framer.push(data) {
        if let Some(ep) = ep {
            fc.routes().ingress(ep, &batch);
        }
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
mod tests;
