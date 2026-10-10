//! PIC-arbiter gate for autonomous injectors on the FC write path.
//!
//! An operator flying by hand and an autonomous plugin driving guided setpoints
//! both reach the flight controller through the same socket. Without a gate the
//! injector's commands and the operator's are indistinguishable, so an
//! autonomous behaviour can fight a human who has taken manual control. This
//! gate closes that: a writer that DECLARES itself an autonomous injector (see
//! [`ados_protocol::ipc::IPC_DECLARE_INJECTOR_PREFIX`]), and any writer whose
//! kernel credentials put it on the plugin plane whatever it declares (see
//! [`ados_protocol::ipc::is_plugin_peer`]), is subject to the PIC arbiter; an
//! operator writer is the human path and is NEVER gated — the load-bearing
//! invariant. A self-declaration alone would only bind cooperating clients.
//!
//! A declared or plugin-plane injector is refused whenever the arbiter reports
//! a human holds manual control, and — fail-closed — whenever the arbiter is
//! not reporting at all. A dead or hung arbiter is not consent.
//!
//! The verification reuses the EXACT `ados-protocol` primitives that `ados-crsf`
//! `InjectorAuth` uses (`crsf_inject_scope` + `WsTicketIssuer` against the
//! pairing key), so the CRSF plane and this MAVLink plane cannot drift on what a
//! valid injector ticket is.
//!
//! # No file I/O on the hot path
//!
//! The PIC sidecar and `pairing.json` are read by a background task
//! ([`GateFeed::run`]) into a `watch` channel: the sidecar every
//! [`PIC_REFRESH`], the pairing file every [`PAIRING_REFRESH`]. The gate
//! ([`InjectorGateCache::refused`]) only reads that channel, so a command never
//! waits on a blocking read on the executor thread. A feed that stopped
//! refreshing is treated as an arbiter that is not reporting (fail closed).
//!
//! # Where the gate runs
//!
//! The arbiter (`ados-pic`) runs only on a ground station, so the gate is built
//! only there (see `MavlinkConfig::injector_arbitration_active`). The feed runs
//! on every profile because the swarm loop also reads the PIC view; where no
//! sidecar exists the view is simply absent.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use ados_hid::pic_view::{injector_refused, read_pic_view, PicView};
use ados_protocol::ipc::InjectorClaim;
use ados_protocol::pairing_posture::{load_pairing, Pairing};
use ados_protocol::shutdown::Shutdown;
use ados_protocol::ws_ticket::{crsf_inject_scope, now_unix, WsTicketIssuer};
use tokio::sync::watch;

/// Verify a declared injector's identity against the pairing key, exactly as
/// `ados-crsf`'s `InjectorAuth::verify` does. Returns the attested `client_id`
/// on success, or `None` when the ticket is missing/invalid — an unverified
/// claim, which can never win the lane away from a human holder. On an unpaired
/// node nothing can mint or check a ticket, so the asserted id is accepted (the
/// lane is not yet protected by anything, matching `InjectorAuth`).
pub fn verify_injector(pairing_path: &Path, claim: &InjectorClaim) -> Option<String> {
    verify_against(&load_pairing(pairing_path), claim)
}

/// [`verify_injector`] against an already-loaded pairing posture.
fn verify_against(pairing: &Pairing, claim: &InjectorClaim) -> Option<String> {
    match pairing {
        Pairing::Unpaired => Some(claim.client_id.clone()),
        // An unreadable pairing file admits no remote injector.
        Pairing::Unreadable => None,
        Pairing::Paired(api_key) => {
            let ticket = claim.ticket.as_deref()?;
            WsTicketIssuer::from_api_key(api_key)
                .verify(ticket, &crsf_inject_scope(&claim.client_id), now_unix())
                .ok()
                .map(|()| claim.client_id.clone())
        }
    }
}

/// The pairing file, respecting the same `ADOS_PAIRING_JSON` override every
/// other surface reads.
fn default_pairing_path() -> PathBuf {
    std::env::var("ADOS_PAIRING_JSON")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/etc/ados/pairing.json"))
}

/// How often the background feed re-reads the PIC sidecar.
pub const PIC_REFRESH: Duration = Duration::from_millis(50);

/// How often the background feed re-reads `pairing.json`. Matches the proxies'
/// pairing TTL: a pair, unpair or key rotation is honoured within it.
pub const PAIRING_REFRESH: Duration = Duration::from_secs(2);

/// A feed sample older than this means the refresh task stopped. The gate then
/// treats the arbiter as not reporting and fails closed.
const FEED_STALE_AFTER: Duration = Duration::from_millis(500);

/// How long a claim's verification is reused before it is checked again. The
/// ticket carries an expiry and the pairing key can be rotated or cleared, so a
/// verification cached for the life of the connection kept authority the
/// ticket no longer grants.
const VERIFY_CACHE_TTL: Duration = Duration::from_secs(5);

/// How many distinct claims keep a cached verification. Two or three injectors
/// alternating is the realistic worst case; eight leaves headroom without
/// letting a claim-spraying writer grow the cache.
const VERIFY_CACHE_CAP: usize = 8;

/// One sample of the gate's inputs, published by [`GateFeed`].
#[derive(Debug, Clone)]
pub struct GateInputs {
    /// The PIC view (`None` when the sidecar is absent, stale or malformed).
    pub pic: Option<PicView>,
    /// The pairing posture the injector tickets are verified against.
    pub pairing: Pairing,
    /// When this sample was taken; `None` before the first refresh.
    pub read_at: Option<Instant>,
}

impl Default for GateInputs {
    fn default() -> Self {
        // Before the first refresh nothing is known: no PIC view (fail closed)
        // and a pairing posture that verifies no remote injector.
        Self {
            pic: None,
            pairing: Pairing::Unreadable,
            read_at: None,
        }
    }
}

impl GateInputs {
    /// Whether this sample is recent enough to act on at `now`.
    fn fresh_at(&self, now: Instant) -> bool {
        self.read_at
            .is_some_and(|at| now.saturating_duration_since(at) <= FEED_STALE_AFTER)
    }
}

/// The background reader of the PIC sidecar and the pairing file.
pub struct GateFeed {
    pairing_path: PathBuf,
    pic_state_path: PathBuf,
    tx: watch::Sender<GateInputs>,
}

impl Default for GateFeed {
    fn default() -> Self {
        Self::new()
    }
}

impl GateFeed {
    /// Resolve the on-box paths once (respecting `ADOS_PAIRING_JSON`).
    pub fn new() -> Self {
        Self::with_paths(default_pairing_path(), ados_hid::paths::pic_state_json())
    }

    /// A feed over explicit paths (tests, alternate run dirs).
    pub fn with_paths(pairing_path: PathBuf, pic_state_path: PathBuf) -> Self {
        let (tx, _) = watch::channel(GateInputs::default());
        Self {
            pairing_path,
            pic_state_path,
            tx,
        }
    }

    /// A receiver of the published samples.
    pub fn subscribe(&self) -> watch::Receiver<GateInputs> {
        self.tx.subscribe()
    }

    /// The PIC view when the sidecar is present and the sample is fresh;
    /// `None` otherwise (absent sidecar, stale sidecar, or a stopped feed).
    pub fn fresh_pic_view(&self, now: Instant) -> Option<PicView> {
        let inputs = self.tx.borrow();
        if !inputs.fresh_at(now) {
            return None;
        }
        inputs.pic.clone()
    }

    /// One blocking refresh: re-read the sidecar, and the pairing file when
    /// `reload_pairing` is set (otherwise the previous posture is kept).
    pub fn refresh(&self, now: Instant, wall: SystemTime, reload_pairing: bool) {
        let pic = read_pic_view(&self.pic_state_path, wall);
        let pairing = if reload_pairing || self.tx.borrow().read_at.is_none() {
            load_pairing(&self.pairing_path)
        } else {
            self.tx.borrow().pairing.clone()
        };
        self.tx.send_replace(GateInputs {
            pic,
            pairing,
            read_at: Some(now),
        });
    }

    /// Refresh on [`PIC_REFRESH`] until `cancel`, re-reading the pairing file
    /// every [`PAIRING_REFRESH`]. Each read runs on the blocking pool.
    pub async fn run(self: std::sync::Arc<Self>, cancel: Shutdown) {
        let mut tick = tokio::time::interval(PIC_REFRESH);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut pairing_read_at: Option<Instant> = None;
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                _ = cancel.wait() => return,
            }
            let now = Instant::now();
            let reload = pairing_read_at
                .is_none_or(|at| now.saturating_duration_since(at) >= PAIRING_REFRESH);
            if reload {
                pairing_read_at = Some(now);
            }
            let feed = self.clone();
            let _ = tokio::task::spawn_blocking(move || {
                feed.refresh(Instant::now(), SystemTime::now(), reload)
            })
            .await;
        }
    }
}

/// The hot-path injector gate: the PIC arbiter's decision
/// ([`injector_refused`]) over the [`GateFeed`]'s latest sample, with each
/// distinct claim's verification cached for [`VERIFY_CACHE_TTL`].
///
/// Held behind a `std::sync::Mutex` on the connection and used synchronously, so
/// the lock never spans an `.await`. It performs no file I/O.
pub struct InjectorGateCache {
    inputs: watch::Receiver<GateInputs>,
    /// Recently verified claims, their attested id, and when each was
    /// verified. A short vector: [`VERIFY_CACHE_CAP`] entries, linear lookup.
    verify_cache: Vec<(InjectorClaim, Option<String>, Instant)>,
    /// Verifications actually computed (cache misses). Observability for the
    /// cache's own behaviour.
    verifications: u64,
}

impl InjectorGateCache {
    /// A gate reading `feed`'s samples.
    pub fn new(feed: &GateFeed) -> Self {
        Self {
            inputs: feed.subscribe(),
            verify_cache: Vec::with_capacity(VERIFY_CACHE_CAP),
            verifications: 0,
        }
    }

    /// Is this declared injector's command REFUSED right now?
    pub fn refused(&mut self, claim: &InjectorClaim) -> bool {
        self.refused_at(claim, Instant::now())
    }

    /// Verifications computed so far (cache misses).
    pub fn verifications(&self) -> u64 {
        self.verifications
    }

    /// [`Self::refused`] with the clock injected.
    fn refused_at(&mut self, claim: &InjectorClaim, now: Instant) -> bool {
        let Self {
            inputs,
            verify_cache,
            verifications,
        } = self;
        let inputs = inputs.borrow();
        if !inputs.fresh_at(now) {
            // The feed stopped: the arbiter's verdict is unknown. Fail closed.
            return true;
        }
        let verified = verified_id(verify_cache, verifications, claim, &inputs.pairing, now);
        injector_refused(inputs.pic.as_ref(), verified.as_deref())
    }
}

/// The attested id for `claim`, from `cache` when fresh; a miss verifies
/// against `pairing`, counts in `verifications` and is cached, evicting the
/// least recently verified entry at [`VERIFY_CACHE_CAP`].
fn verified_id(
    cache: &mut Vec<(InjectorClaim, Option<String>, Instant)>,
    verifications: &mut u64,
    claim: &InjectorClaim,
    pairing: &Pairing,
    now: Instant,
) -> Option<String> {
    if let Some((_, id, _)) = cache
        .iter()
        .find(|(c, _, at)| c == claim && now.saturating_duration_since(*at) < VERIFY_CACHE_TTL)
    {
        return id.clone();
    }
    let id = verify_against(pairing, claim);
    *verifications += 1;
    cache.retain(|(c, _, _)| c != claim);
    if cache.len() >= VERIFY_CACHE_CAP {
        if let Some(oldest) = cache
            .iter()
            .enumerate()
            .min_by_key(|(_, (_, _, at))| *at)
            .map(|(i, _)| i)
        {
            cache.swap_remove(oldest);
        }
    }
    cache.push((claim.clone(), id.clone(), now));
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(id: &str, ticket: Option<&str>) -> InjectorClaim {
        InjectorClaim {
            client_id: id.to_string(),
            ticket: ticket.map(str::to_string),
        }
    }

    // ── verification against the pairing key ─────────────────────────────────

    #[test]
    fn unpaired_node_accepts_the_asserted_id() {
        let dir = tempfile::tempdir().unwrap();
        let pairing = dir.path().join("pairing.json");
        // No pairing file → Unpaired → asserted id accepted, no ticket needed.
        assert_eq!(
            verify_injector(&pairing, &claim("ai-mission", None)),
            Some("ai-mission".to_string())
        );
    }

    #[test]
    fn paired_node_rejects_a_missing_or_bad_ticket() {
        let dir = tempfile::tempdir().unwrap();
        let pairing = dir.path().join("pairing.json");
        std::fs::write(&pairing, r#"{"paired": true, "api_key": "k-secret"}"#).unwrap();
        assert_eq!(verify_injector(&pairing, &claim("ai-mission", None)), None);
        assert_eq!(
            verify_injector(&pairing, &claim("ai-mission", Some("not-a-real-ticket"))),
            None
        );
    }

    #[test]
    fn paired_node_accepts_a_valid_scoped_ticket() {
        use ados_protocol::ws_ticket::mint_scoped_ticket;
        let dir = tempfile::tempdir().unwrap();
        let pairing = dir.path().join("pairing.json");
        std::fs::write(&pairing, r#"{"paired": true, "api_key": "k-secret"}"#).unwrap();
        let ticket = mint_scoped_ticket(&pairing, &crsf_inject_scope("ai-mission"), 300)
            .expect("mint a ticket on a paired node");
        assert_eq!(
            verify_injector(&pairing, &claim("ai-mission", Some(&ticket))),
            Some("ai-mission".to_string())
        );
        // The same ticket does NOT verify for a DIFFERENT client id (scope-bound).
        assert_eq!(
            verify_injector(&pairing, &claim("other", Some(&ticket))),
            None
        );
    }

    // ── the feed and the hot-path cache ──────────────────────────────────────

    fn feed_in(dir: &Path) -> GateFeed {
        GateFeed::with_paths(dir.join("pairing.json"), dir.join("pic-state.json"))
    }

    /// Write the PIC sidecar in the real schema `read_pic_view` parses.
    fn write_pic(dir: &Path, holder: Option<&str>) {
        let body = match holder {
            Some(h) => format!(r#"{{"state": "claimed", "claimed_by": "{h}"}}"#),
            None => r#"{"state": "unclaimed"}"#.to_string(),
        };
        std::fs::write(dir.join("pic-state.json"), body).unwrap();
    }

    /// The gate reads only the feed: a sidecar change is invisible until the
    /// feed refreshes, and then the verdict follows it.
    #[test]
    fn the_gate_follows_the_feed_not_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let feed = feed_in(dir.path());
        let mut gate = InjectorGateCache::new(&feed);
        let c = claim("ai-mission", None);

        write_pic(dir.path(), None);
        let t0 = Instant::now();
        feed.refresh(t0, SystemTime::now(), true);
        assert!(
            !gate.refused_at(&c, t0),
            "unclaimed PIC allows the injector"
        );

        // The operator grabs control; the file changes but the feed has not
        // refreshed, so the gate still sees the previous sample.
        write_pic(dir.path(), Some("hdmi-kiosk"));
        assert!(!gate.refused_at(&c, t0 + Duration::from_millis(10)));

        // The next refresh carries the grab, and the injector is refused.
        let t1 = t0 + PIC_REFRESH;
        feed.refresh(t1, SystemTime::now(), false);
        assert!(gate.refused_at(&c, t1), "the operator's grab refuses");
    }

    /// No sample yet, or a feed that stopped refreshing, is an arbiter that is
    /// not reporting: refused.
    #[test]
    fn a_missing_or_stopped_feed_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let feed = feed_in(dir.path());
        let mut gate = InjectorGateCache::new(&feed);
        let c = claim("ai-mission", None);
        let t0 = Instant::now();
        assert!(gate.refused_at(&c, t0), "no sample yet");

        write_pic(dir.path(), None);
        feed.refresh(t0, SystemTime::now(), true);
        assert!(!gate.refused_at(&c, t0));
        assert!(
            gate.refused_at(&c, t0 + FEED_STALE_AFTER + Duration::from_millis(1)),
            "a stale feed must not keep granting the lane"
        );
    }

    /// Two injectors alternating used to thrash a single-slot cache, re-reading
    /// the pairing file and recomputing the HMAC on every switch.
    #[test]
    fn alternating_claims_verify_at_most_once_each_per_ttl() {
        let dir = tempfile::tempdir().unwrap();
        let feed = feed_in(dir.path());
        write_pic(dir.path(), None);
        let t0 = Instant::now();
        feed.refresh(t0, SystemTime::now(), true);
        let mut gate = InjectorGateCache::new(&feed);
        let (a, b) = (claim("ai-a", None), claim("ai-b", None));
        for i in 0..100u32 {
            let now = t0 + Duration::from_millis(u64::from(i));
            gate.refused_at(if i % 2 == 0 { &a } else { &b }, now);
        }
        assert_eq!(gate.verifications(), 2, "one verify per distinct claim");
    }

    #[test]
    fn the_verify_cache_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let feed = feed_in(dir.path());
        write_pic(dir.path(), None);
        let t0 = Instant::now();
        feed.refresh(t0, SystemTime::now(), true);
        let mut gate = InjectorGateCache::new(&feed);
        for i in 0..20 {
            gate.refused_at(&claim(&format!("c{i}"), None), t0);
        }
        assert!(gate.verify_cache.len() <= VERIFY_CACHE_CAP);
    }

    /// The verify is resolved per distinct claim: a paired node with no ticket
    /// can never win the lane, whatever id it asserts.
    #[test]
    fn verify_is_resolved_per_distinct_claim() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pairing.json"),
            r#"{"paired": true, "api_key": "k-secret"}"#,
        )
        .unwrap();
        write_pic(dir.path(), Some("ai-mission"));
        let feed = feed_in(dir.path());
        let now = Instant::now();
        feed.refresh(now, SystemTime::now(), true);
        let mut gate = InjectorGateCache::new(&feed);
        assert!(gate.refused_at(&claim("ai-mission", None), now));
        assert!(gate.refused_at(&claim("other", None), now));
        assert_eq!(gate.verifications(), 2);
    }

    /// A verification is not kept for the life of the lane: once the node is
    /// paired after the injector connected, the same claim is checked against
    /// the new key and loses the lane it can no longer prove.
    #[test]
    fn a_cached_verification_expires_and_is_rechecked() {
        let dir = tempfile::tempdir().unwrap();
        write_pic(dir.path(), Some("ai-mission"));
        let feed = feed_in(dir.path());
        let t0 = Instant::now();
        feed.refresh(t0, SystemTime::now(), true);
        let mut gate = InjectorGateCache::new(&feed);
        let c = claim("ai-mission", None);
        assert!(!gate.refused_at(&c, t0), "unpaired: the asserted id holds");

        std::fs::write(
            dir.path().join("pairing.json"),
            r#"{"paired": true, "api_key": "k-secret"}"#,
        )
        .unwrap();
        let later = t0 + VERIFY_CACHE_TTL + Duration::from_millis(1);
        feed.refresh(later, SystemTime::now(), true);
        assert!(
            gate.refused_at(&c, later),
            "an expired verification must be rechecked against the current key"
        );
    }

    #[test]
    fn fresh_pic_view_is_absent_without_a_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let feed = feed_in(dir.path());
        let now = Instant::now();
        feed.refresh(now, SystemTime::now(), true);
        assert_eq!(feed.fresh_pic_view(now), None);
        write_pic(dir.path(), Some("op-a"));
        feed.refresh(now, SystemTime::now(), false);
        assert_eq!(
            feed.fresh_pic_view(now).and_then(|v| v.holder),
            Some("op-a".to_string())
        );
    }
}
