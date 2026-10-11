//! The regulatory domain: one `iw reg get` reading, taken off the caller's path.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long one `iw reg get` reading is served before a refresh is scheduled.
///
/// The regulatory domain changes only when something explicitly sets it — the
/// radio's bring-up gate and the supervisor's reconciler both issue
/// `iw reg set` — never on its own, while the GCS radio panel polls the status
/// route about once a second and the cloud heartbeat ticks every few seconds.
/// Serving one reading for this window keeps a real domain change visible
/// inside it without taking a reading per request. The value stays the freshest
/// thing in the body by a wide margin: the link figures beside it come off a
/// sidecar the route only calls stale after ten seconds.
pub(super) const REG_DOMAIN_TTL: Duration = Duration::from_secs(3);

/// Upper bound on one `iw reg get`.
///
/// A wedged RTL driver — the failure `rtl_modprobe` exists to recover from —
/// can leave `iw` blocked in the kernel. Without this bound the refresh task
/// never completes, the in-flight flag never clears, and the served reading
/// freezes at whatever was true before the wedge instead of degrading to
/// `"unknown"`.
pub(super) const REG_DOMAIN_READ_TIMEOUT: Duration = Duration::from_secs(2);

/// What is served when no reading has landed yet, and when a read fails.
///
/// Never a plausible country this process did not read: a status surface
/// reporting `"US"` it never measured is the same defect class as a fabricated
/// RSSI, and a regulatory domain is what an operator checks before radiating.
pub(super) const REG_DOMAIN_UNKNOWN: &str = "unknown";

/// One string reading, served without ever taking it on the caller's path.
///
/// What it serves is always a reading this process genuinely took, or the
/// honest [`REG_DOMAIN_UNKNOWN`] when none has landed; nothing is synthesised,
/// and a failed read has its own value which is cached like any other, so a
/// wedged `iw` is not retried once per request.
pub(super) struct RegDomainCache {
    held: Mutex<Option<(Instant, String)>>,
    /// Set while a refresh is in flight, so a burst of concurrent polls
    /// schedules one `iw` rather than one per request.
    refreshing: AtomicBool,
}

impl RegDomainCache {
    pub(super) const fn new() -> Self {
        Self {
            held: Mutex::new(None),
            refreshing: AtomicBool::new(false),
        }
    }

    /// The held reading, and whether it is due a refresh.
    ///
    /// A poisoned lock means an earlier holder panicked mid-update; report no
    /// reading rather than propagate the panic, because the routes behind this
    /// are required to answer.
    pub(super) fn peek_at(&self, now: Instant, ttl: Duration) -> (String, bool) {
        match self.held.lock() {
            Ok(held) => match held.as_ref() {
                Some((at, value)) if now.duration_since(*at) < ttl => (value.clone(), false),
                Some((_, value)) => (value.clone(), true),
                None => (REG_DOMAIN_UNKNOWN.to_string(), true),
            },
            Err(_) => (REG_DOMAIN_UNKNOWN.to_string(), true),
        }
    }

    pub(super) fn store_at(&self, now: Instant, value: String) {
        if let Ok(mut held) = self.held.lock() {
            *held = Some((now, value));
        }
    }

    /// True when this caller took the right to run the one in-flight refresh.
    pub(super) fn claim_refresh(&self) -> bool {
        self.refreshing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(super) fn release_refresh(&self) {
        self.refreshing.store(false, Ordering::Release);
    }
}

/// The process-wide reading every status caller shares.
static REG_DOMAIN_CACHE: RegDomainCache = RegDomainCache::new();

/// The last regulatory-domain reading, scheduling a refresh when it is due.
///
/// Never blocks and never spawns a process on the calling thread: past the TTL
/// the read goes onto the current tokio runtime and this returns the reading it
/// already holds (or `"unknown"` before the first one lands, which self-heals
/// on the next poll). A caller that wants the value present in its very first
/// response seeds it by awaiting [`refresh_regulatory_domain`] at startup.
pub fn regulatory_domain() -> String {
    let (value, due) = REG_DOMAIN_CACHE.peek_at(Instant::now(), REG_DOMAIN_TTL);
    if due {
        schedule_regulatory_refresh();
    }
    value
}

/// Take a reading now and hold it, for a caller already on an async path.
pub async fn refresh_regulatory_domain() -> String {
    let value = read_regulatory_domain().await;
    REG_DOMAIN_CACHE.store_at(Instant::now(), value.clone());
    value
}

/// Put the one in-flight `iw reg get` on the current runtime, if there is one.
///
/// Off-runtime (a unit test, a sync tool) there is nothing to spawn onto, so no
/// reading is taken and the caller keeps the honest `"unknown"` — the blocking
/// fork is never smuggled back onto the calling thread as a fallback.
pub(super) fn schedule_regulatory_refresh() {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    if !REG_DOMAIN_CACHE.claim_refresh() {
        return;
    }
    handle.spawn(async {
        let value = read_regulatory_domain().await;
        REG_DOMAIN_CACHE.store_at(Instant::now(), value);
        REG_DOMAIN_CACHE.release_refresh();
    });
}

/// Bounded `iw reg get`, parsed by [`parse_reg_get`]. `"unknown"` on a spawn
/// failure, a non-zero exit, or the timeout.
///
/// `kill_on_drop` carries as much of the bound as the timeout does: without it
/// the timeout would leave a wedged `iw` behind as an unreaped child on every
/// refresh, which is how a bounded-looking probe still exhausts a box.
pub(super) async fn read_regulatory_domain() -> String {
    let reading = tokio::process::Command::new("iw")
        .args(["reg", "get"])
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(REG_DOMAIN_READ_TIMEOUT, reading).await {
        Ok(Ok(out)) if out.status.success() => parse_reg_get(&String::from_utf8_lossy(&out.stdout)),
        _ => REG_DOMAIN_UNKNOWN.to_string(),
    }
}

/// The two-letter country code, `"global"`, or `"unknown"` from `iw reg get`
/// output. Pure, so the parse is a unit under test on a host without `iw`.
pub(super) fn parse_reg_get(stdout: &str) -> String {
    for line in stdout.lines() {
        let stripped = line.trim();
        if let Some(rest) = stripped.strip_prefix("country ") {
            // Format: "country US: DFS-FCC" — keep the two-letter code.
            let code = rest.split(':').next().unwrap_or("").trim();
            if code.is_empty() {
                return REG_DOMAIN_UNKNOWN.to_string();
            }
            return code.to_string();
        }
        if stripped.starts_with("global") {
            return "global".to_string();
        }
    }
    REG_DOMAIN_UNKNOWN.to_string()
}
