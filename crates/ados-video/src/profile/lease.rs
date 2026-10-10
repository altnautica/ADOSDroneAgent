//! Hero leases: time-bounded claims on the hero profile, composed with the
//! base profile inside [`EncoderControl`].
//!
//! The base profile is what the node runs with no claim on it: the boot
//! profile, or the last plain set (`video.encoder.profile.set`, which is what
//! the ground station's fleet-hero assignment sends). A lease is one holder's
//! claim on hero:
//!
//! ```text
//! effective profile = hero   while any lease is live
//!                   = base   otherwise
//! ```
//!
//! A remote holder renews its lease before it runs out; one that stops
//! renewing (a closed viewer, a dropped link) is dropped by
//! [`run_lease_expiry`] with no further request, and the encoder reverts to
//! the base. A lease with no expiry is internal only: the vision tap holds
//! [`VISION_LEASE_HOLDER`] while an engine is attached, so the tap never reads
//! a thumbnail-rate stream.
//!
//! Every lease change publishes a new desired generation, so the orchestrator
//! re-resolves and republishes the sidecar. It respawns the encoder only when
//! the resolved settings differ from what is live, so a renewal, or a second
//! holder joining an already-hero drone, touches no process.

use std::collections::BTreeMap;
use std::sync::{Arc, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::{EncoderControl, EncoderState, VideoProfile};

/// Shortest lease a remote holder may request, in seconds.
pub const LEASE_MIN_S: u64 = 5;

/// Longest lease a remote holder may request, in seconds.
pub const LEASE_MAX_S: u64 = 120;

/// The lease the HTTP route grants when the request names none, in seconds.
pub const LEASE_DEFAULT_S: u64 = 30;

/// Longest accepted holder id, in characters.
pub const LEASE_HOLDER_MAX_LEN: usize = 64;

/// The internal holder the vision tap uses while an engine is attached. Not
/// accepted from a remote caller, so no request can release it.
pub const VISION_LEASE_HOLDER: &str = "vision";

/// How often [`run_lease_expiry`] drops expired leases.
pub const LEASE_SWEEP_INTERVAL: Duration = Duration::from_secs(1);

/// The monotonic time source lease deadlines are measured against.
pub trait MonotonicClock: Send + Sync {
    fn now(&self) -> Instant;
}

/// [`MonotonicClock`] over [`Instant::now`].
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl MonotonicClock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// One lease as reported on the command socket, the sidecar and the HTTP
/// route.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeaseView {
    pub holder: String,
    /// Seconds until the lease ends, or `None` for a lease with no expiry.
    pub expires_in_s: Option<f64>,
}

/// Validate a remote holder id: 1-64 characters of `[A-Za-z0-9._:-]`, and
/// not the reserved [`VISION_LEASE_HOLDER`]. The error is the operator-facing
/// detail.
pub fn check_lease_holder(holder: &str) -> Result<(), &'static str> {
    let well_formed = !holder.is_empty()
        && holder.len() <= LEASE_HOLDER_MAX_LEN
        && holder.bytes().all(holder_byte_ok);
    if !well_formed {
        return Err("holder must be 1-64 characters of [A-Za-z0-9._:-]");
    }
    if holder == VISION_LEASE_HOLDER {
        return Err("holder \"vision\" is reserved for the on-board vision tap");
    }
    Ok(())
}

fn holder_byte_ok(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-')
}

/// Whether `lease_s` is inside the accepted `LEASE_MIN_S..=LEASE_MAX_S` range.
pub fn lease_seconds_in_range(lease_s: u64) -> bool {
    (LEASE_MIN_S..=LEASE_MAX_S).contains(&lease_s)
}

/// The base profile and the live leases, behind [`EncoderControl`]'s lock.
pub(super) struct LeaseTable {
    pub(super) base: VideoProfile,
    /// Holder to deadline; `None` never expires.
    leases: BTreeMap<String, Option<Instant>>,
    clock: Arc<dyn MonotonicClock>,
}

impl std::fmt::Debug for LeaseTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeaseTable")
            .field("base", &self.base)
            .field("leases", &self.leases)
            .finish_non_exhaustive()
    }
}

impl LeaseTable {
    pub(super) fn new(base: VideoProfile, clock: Arc<dyn MonotonicClock>) -> Self {
        Self {
            base,
            leases: BTreeMap::new(),
            clock,
        }
    }

    /// Drop every lease whose deadline has passed. True when one was dropped.
    fn sweep(&mut self) -> bool {
        let now = self.clock.now();
        let before = self.leases.len();
        self.leases.retain(|_, d| d.is_none_or(|t| t > now));
        self.leases.len() != before
    }

    fn effective(&self) -> VideoProfile {
        if self.leases.is_empty() {
            self.base
        } else {
            VideoProfile::Hero
        }
    }

    fn views(&self) -> Vec<LeaseView> {
        let now = self.clock.now();
        let remaining = |d: Instant| d.saturating_duration_since(now).as_secs_f64();
        self.leases
            .iter()
            .map(|(holder, deadline)| LeaseView {
                holder: holder.clone(),
                expires_in_s: deadline.map(remaining),
            })
            .collect()
    }
}

/// Releases its lease when dropped, including when the owning task is
/// aborted.
#[derive(Debug)]
pub struct LeaseGuard {
    control: Arc<EncoderControl>,
    holder: &'static str,
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        self.control.release_lease(self.holder);
    }
}

impl EncoderControl {
    fn lock_leases(&self) -> MutexGuard<'_, LeaseTable> {
        self.leases.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Apply `change` to the lease table (after dropping expired leases), then
    /// publish the resulting effective profile as a new desired generation.
    pub(super) fn compose(&self, change: impl FnOnce(&mut LeaseTable)) -> u64 {
        let mut table = self.lock_leases();
        table.sweep();
        change(&mut table);
        let profile = table.effective();
        self.bump(|d| d.profile = profile)
    }

    /// Stamp the base profile and the live leases onto `state`.
    pub(super) fn stamp_attention(&self, mut state: EncoderState) -> EncoderState {
        let table = self.lock_leases();
        state.base_profile = Some(table.base);
        state.leases = table.views();
        state
    }

    /// Create or renew `holder`'s hero lease for `ttl` from now. Returns the
    /// generation to wait on.
    pub fn set_lease(&self, holder: &str, ttl: Duration) -> u64 {
        self.compose(|t| {
            let deadline = t.clock.now() + ttl;
            t.leases.insert(holder.to_string(), Some(deadline));
        })
    }

    /// Release `holder`'s lease. Releasing an unknown holder is not an error.
    /// Returns the generation to wait on.
    pub fn release_lease(&self, holder: &str) -> u64 {
        self.compose(|t| {
            t.leases.remove(holder);
        })
    }

    /// Hold a lease with no expiry until the returned guard drops. Returns the
    /// guard and the generation to wait on.
    pub fn hold_lease(self: &Arc<Self>, holder: &'static str) -> (LeaseGuard, u64) {
        let generation = self.compose(|t| {
            t.leases.insert(holder.to_string(), None);
        });
        let guard = LeaseGuard {
            control: Arc::clone(self),
            holder,
        };
        (guard, generation)
    }

    /// Drop expired leases and re-resolve. `Some(generation)` when a lease was
    /// dropped; `None` (and no new generation) when nothing had expired.
    pub fn sweep_expired_leases(&self) -> Option<u64> {
        let mut table = self.lock_leases();
        if !table.sweep() {
            return None;
        }
        let profile = table.effective();
        Some(self.bump(|d| d.profile = profile))
    }

    /// The base profile: what the node runs with no lease live.
    pub fn base_profile(&self) -> VideoProfile {
        self.lock_leases().base
    }

    /// The leases currently held, by holder.
    pub fn leases(&self) -> Vec<LeaseView> {
        self.lock_leases().views()
    }
}

/// Drop expired leases every [`LEASE_SWEEP_INTERVAL`] until `shutdown` fires,
/// so an abandoned lease reverts the encoder without any further request.
pub async fn run_lease_expiry(
    control: Arc<EncoderControl>,
    shutdown: ados_protocol::shutdown::Shutdown,
) {
    let mut tick = tokio::time::interval(LEASE_SWEEP_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = shutdown.wait() => break,
            _ = tick.tick() => {
                if control.sweep_expired_leases().is_some() {
                    tracing::info!(
                        profile = %control.desired().profile,
                        base_profile = %control.base_profile(),
                        "video_lease_expired"
                    );
                }
            }
        }
    }
}

/// Test helpers shared by the profile, IPC and vision-tap tests.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::MonotonicClock;
    use crate::config::CameraConfig;
    use crate::profile::{resolve, EncoderControl, EncoderState};

    /// A clock that moves only when told to.
    #[derive(Debug)]
    pub(crate) struct ManualClock {
        origin: Instant,
        offset_ms: AtomicU64,
    }

    impl ManualClock {
        pub(crate) fn new() -> Arc<Self> {
            Arc::new(Self {
                origin: Instant::now(),
                offset_ms: AtomicU64::new(0),
            })
        }

        pub(crate) fn advance(&self, by: Duration) {
            let ms = by.as_millis() as u64;
            self.offset_ms.fetch_add(ms, Ordering::SeqCst);
        }
    }

    impl MonotonicClock for ManualClock {
        fn now(&self) -> Instant {
            self.origin + Duration::from_millis(self.offset_ms.load(Ordering::SeqCst))
        }
    }

    /// Stand in for the orchestrator: acknowledge every desired change by
    /// resolving it against the default camera config. Subscribes on the
    /// caller's thread so a request issued right after this returns is seen.
    pub(crate) fn spawn_applier(control: Arc<EncoderControl>) -> tokio::task::JoinHandle<()> {
        let mut rx = control.subscribe_desired();
        tokio::spawn(async move {
            let cfg = CameraConfig::default();
            while rx.changed().await.is_ok() {
                let d = *rx.borrow_and_update();
                let s = resolve(d.profile, d.ceiling_kbps, &cfg);
                control.note_applied(
                    EncoderState::new(d.profile, d.ceiling_kbps, s),
                    d.generation,
                    true,
                );
            }
        })
    }
}

#[cfg(test)]
#[path = "lease_tests.rs"]
mod tests;
