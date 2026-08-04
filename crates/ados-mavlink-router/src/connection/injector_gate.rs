//! PIC-arbiter gate for autonomous injectors on the FC write path.
//!
//! An operator flying by hand and an autonomous plugin driving guided setpoints
//! both reach the flight controller through the same socket. Without a gate the
//! injector's commands and the operator's are indistinguishable, so an
//! autonomous behaviour can fight a human who has taken manual control. This
//! gate closes that: a writer that DECLARES itself an autonomous injector (see
//! [`ados_protocol::ipc::IPC_DECLARE_INJECTOR_PREFIX`]) is subject to the PIC
//! arbiter; a writer that declares nothing is the operator/human path and is
//! NEVER gated — the load-bearing invariant.
//!
//! Ships INERT: nothing declares injector until a producer is armed, so no
//! command carries a claim and this gate never runs. When a producer is armed
//! (opt-in on a deployment with a PIC arbiter), a declared injector is refused
//! whenever the arbiter reports a human holds manual control, and — fail-closed
//! — whenever the arbiter is not reporting at all. A dead or hung arbiter is not
//! consent.
//!
//! The verification reuses the EXACT `ados-protocol` primitives that `ados-crsf`
//! `InjectorAuth` uses (`crsf_inject_scope` + `WsTicketIssuer` against the
//! pairing key), so the CRSF plane and this MAVLink plane cannot drift on what a
//! valid injector ticket is.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use ados_hid::pic_view::{read_pic_view, resolve_authority, Authority, ChannelSourceMode, PicView};
use ados_protocol::ipc::InjectorClaim;
use ados_protocol::pairing_posture::{load_pairing, Pairing};
use ados_protocol::ws_ticket::{crsf_inject_scope, now_unix, WsTicketIssuer};

/// Verify a declared injector's identity against the pairing key, exactly as
/// `ados-crsf`'s `InjectorAuth::verify` does. Returns the attested `client_id`
/// on success, or `None` when the ticket is missing/invalid — an unverified
/// claim, which can never win the lane away from a human holder. On an unpaired
/// node nothing can mint or check a ticket, so the asserted id is accepted (the
/// lane is not yet protected by anything, matching `InjectorAuth`).
pub fn verify_injector(pairing_path: &Path, claim: &InjectorClaim) -> Option<String> {
    match load_pairing(pairing_path) {
        Pairing::Unpaired => Some(claim.client_id.clone()),
        Pairing::Paired(api_key) => {
            let ticket = claim.ticket.as_deref()?;
            WsTicketIssuer::from_api_key(&api_key)
                .verify(ticket, &crsf_inject_scope(&claim.client_id), now_unix())
                .ok()
                .map(|()| claim.client_id.clone())
        }
    }
}

/// The pure gate decision: does a declared injector's command get REFUSED, given
/// the PIC arbiter's view and the injector's verified identity? Pure over its
/// inputs so the full matrix is unit-testable with no filesystem.
///
/// This is exactly [`resolve_authority`] in `Hybrid` mode reading `Authority::Hid`
/// (the human/neutral hold) — reused rather than re-spelled so the FC-write plane
/// and the CRSF channel plane arbitrate identically. `Hid` (refuse) covers: a
/// human holds the claim, a claim with no/other holder, and a non-reporting
/// arbiter (fail-closed). `Inject` (allow) covers: the injector itself holds the
/// claim, or a fresh affirmative "no one holds".
pub fn injector_refused_decision(pic: Option<&PicView>, verified_injector: Option<&str>) -> bool {
    resolve_authority(ChannelSourceMode::Hybrid, pic, verified_injector) == Authority::Hid
}

/// The pairing file, respecting the same `ADOS_PAIRING_JSON` override every
/// other surface reads.
fn default_pairing_path() -> PathBuf {
    std::env::var("ADOS_PAIRING_JSON")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/etc/ados/pairing.json"))
}

/// How long a PIC read is reused before the sidecar is re-read on the hot path.
/// Mirrors the attitude rung's once-per-tick model (its `RATE_PERIOD` is 50 ms):
/// a fresh operator grab is honored within this window, and the per-command
/// blocking read stays off the async router loop.
const PIC_CACHE_TTL: Duration = Duration::from_millis(50);

/// The hot-path injector gate: the same decision as [`injector_refused`], but
/// with the two blocking costs cached off the async router loop.
///
/// [`injector_refused`] read `pairing.json` (+ an HMAC verify) AND the PIC
/// sidecar on EVERY command. At an armed 50–100 Hz `set_raw_rc` cadence that is
/// two blocking `std::fs` reads plus a crypto verify per frame, on the executor
/// thread that also drives every other socket. This caches both:
///
/// * the **verify** is reused while the claim is byte-identical — the injector
///   ticket is minted once per connection, so a claim that verified once stays
///   verified; the HMAC + pairing read run once per distinct claim, not per
///   command.
/// * the **PIC read** is reused within [`PIC_CACHE_TTL`], exactly as the
///   attitude rung reads it once per tick. `None` (absent/stale/malformed) still
///   fails closed to the human hold.
///
/// Held behind a `std::sync::Mutex` on the connection and used synchronously, so
/// the lock never spans an `.await`.
pub struct InjectorGateCache {
    pairing_path: PathBuf,
    pic_state_path: PathBuf,
    /// The last claim verified and its attested id (the injector ticket is
    /// per-connection, so this is sticky for the life of the lane).
    verify_cache: Option<(InjectorClaim, Option<String>)>,
    /// The last PIC read and the instant it was taken.
    pic_cache: Option<(Instant, Option<PicView>)>,
}

impl Default for InjectorGateCache {
    fn default() -> Self {
        Self::new()
    }
}

impl InjectorGateCache {
    /// Resolve the on-box paths once (respecting `ADOS_PAIRING_JSON`), so the hot
    /// path never re-resolves them.
    pub fn new() -> Self {
        Self {
            pairing_path: default_pairing_path(),
            pic_state_path: ados_hid::paths::pic_state_json(),
            verify_cache: None,
            pic_cache: None,
        }
    }

    #[cfg(test)]
    fn with_paths(pairing_path: PathBuf, pic_state_path: PathBuf) -> Self {
        Self {
            pairing_path,
            pic_state_path,
            verify_cache: None,
            pic_cache: None,
        }
    }

    /// Is this declared injector's command REFUSED right now? Caches the verify
    /// per sticky claim and the PIC read on a short TTL; the verdict itself is the
    /// same pure [`injector_refused_decision`] the matrix tests cover.
    pub fn refused(&mut self, claim: &InjectorClaim) -> bool {
        self.refused_at(claim, Instant::now(), SystemTime::now())
    }

    /// [`Self::refused`] with the clocks injected, so the cache behaviour is
    /// unit-testable without sleeping.
    fn refused_at(&mut self, claim: &InjectorClaim, now: Instant, wall: SystemTime) -> bool {
        let verified = match &self.verify_cache {
            Some((cached, id)) if cached == claim => id.clone(),
            _ => {
                let id = verify_injector(&self.pairing_path, claim);
                self.verify_cache = Some((claim.clone(), id.clone()));
                id
            }
        };
        let pic = match &self.pic_cache {
            Some((taken, p)) if now.duration_since(*taken) < PIC_CACHE_TTL => p.clone(),
            _ => {
                let p = read_pic_view(&self.pic_state_path, wall);
                self.pic_cache = Some((now, p.clone()));
                p
            }
        };
        injector_refused_decision(pic.as_ref(), verified.as_deref())
    }
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

    // ── the pure decision matrix ─────────────────────────────────────────────

    #[test]
    fn no_arbiter_report_refuses_the_injector_fail_closed() {
        // pic = None (absent/stale/malformed): a dead arbiter is not consent.
        assert!(injector_refused_decision(None, Some("ai-mission")));
        assert!(injector_refused_decision(None, None));
    }

    #[test]
    fn a_human_holder_refuses_the_injector() {
        let human = PicView {
            claimed: true,
            holder: Some("hdmi-kiosk".into()),
        };
        assert!(injector_refused_decision(Some(&human), Some("ai-mission")));
    }

    #[test]
    fn the_injector_holding_the_claim_is_allowed() {
        let robot = PicView {
            claimed: true,
            holder: Some("ai-mission".into()),
        };
        assert!(!injector_refused_decision(Some(&robot), Some("ai-mission")));
    }

    #[test]
    fn a_fresh_unclaimed_report_allows_the_injector() {
        let unclaimed = PicView::default();
        assert!(!injector_refused_decision(Some(&unclaimed), None));
    }

    #[test]
    fn an_unverified_injector_never_wins_even_holding_the_claim() {
        // holder matches the asserted id, but verified_injector is None (bad
        // ticket): the claim cannot be credited, so a human-path hold stands.
        let claimed_by_asserted = PicView {
            claimed: true,
            holder: Some("ai-mission".into()),
        };
        assert!(injector_refused_decision(Some(&claimed_by_asserted), None));
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
        // No ticket → rejected.
        assert_eq!(verify_injector(&pairing, &claim("ai-mission", None)), None);
        // A garbage ticket → rejected.
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
        // Mint a ticket for the exact injector scope (the same helper ados-crsf
        // mints with), then verify it round-trips.
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

    // ── the hot-path cache ───────────────────────────────────────────────────

    fn unpaired_cache() -> (tempfile::TempDir, InjectorGateCache) {
        let dir = tempfile::tempdir().unwrap();
        // No pairing file → Unpaired → the asserted id is accepted with no ticket,
        // so the verdict turns purely on the PIC sidecar (which is what we vary).
        let cache = InjectorGateCache::with_paths(
            dir.path().join("pairing.json"),
            dir.path().join("pic-state.json"),
        );
        (dir, cache)
    }

    /// Write the PIC sidecar in the real schema `read_pic_view` parses
    /// (`state`/`claimed_by`, NOT `claimed`/`holder`).
    fn write_pic(dir: &Path, holder: Option<&str>) {
        let body = match holder {
            Some(h) => format!(r#"{{"state": "claimed", "claimed_by": "{h}"}}"#),
            None => r#"{"state": "unclaimed"}"#.to_string(),
        };
        std::fs::write(dir.join("pic-state.json"), body).unwrap();
    }

    /// The PIC read is cached within the TTL, then refreshed — so a fresh grab is
    /// honored on the next tick, not the next command, and the blocking read is
    /// off the per-command path. Proven-to-bite: flip the sidecar to a human
    /// holder mid-window and the verdict must NOT change until the TTL elapses,
    /// then MUST flip to refused.
    #[test]
    fn pic_read_is_cached_within_the_ttl_then_refreshes() {
        let (dir, mut cache) = unpaired_cache();
        let c = claim("ai-mission", None);

        // t0: no one holds PIC → the injector is allowed (not refused). A fresh
        // `wall` per call keeps the just-written sidecar inside the staleness gate,
        // while the fake `Instant`s drive the cache TTL independently.
        write_pic(dir.path(), None);
        let t0 = Instant::now();
        assert!(
            !cache.refused_at(&c, t0, SystemTime::now()),
            "unclaimed PIC allows the injector"
        );

        // The operator grabs control, but only 10 ms (< TTL) has passed: the
        // cached "unclaimed" still stands. This is the deliberate, bounded window.
        write_pic(dir.path(), Some("hdmi-kiosk"));
        assert!(
            !cache.refused_at(&c, t0 + Duration::from_millis(10), SystemTime::now()),
            "within the TTL the cached PIC read stands (bounded staleness)"
        );

        // Past the TTL: the sidecar is re-read and the human hold now refuses
        // (holder 'hdmi-kiosk' != the injector's verified id 'ai-mission').
        assert!(
            cache.refused_at(
                &c,
                t0 + PIC_CACHE_TTL + Duration::from_millis(1),
                SystemTime::now()
            ),
            "past the TTL the fresh read sees the operator's grab and refuses"
        );
    }

    /// The verify (pairing read + HMAC on a paired node) is reused while the claim
    /// is byte-identical, and re-run when the claim changes — the sticky-claim
    /// optimisation. Proven-to-bite by asserting the verdict tracks a changed
    /// claim: a paired node with no ticket must be refused (unverified → can't win
    /// the lane), while the unpaired-accepted id turns on the PIC alone.
    #[test]
    fn verify_is_resolved_per_distinct_claim() {
        let dir = tempfile::tempdir().unwrap();
        let pairing = dir.path().join("pairing.json");
        std::fs::write(&pairing, r#"{"paired": true, "api_key": "k-secret"}"#).unwrap();
        write_pic(dir.path(), Some("ai-mission")); // the robot itself holds PIC
        let mut cache =
            InjectorGateCache::with_paths(pairing, dir.path().join("pic-state.json"));

        let now = Instant::now();
        // A claim with no ticket on a PAIRED node cannot verify → unverified →
        // refused even though the holder matches the asserted id.
        assert!(
            cache.refused_at(&claim("ai-mission", None), now, SystemTime::now()),
            "an unverified claim can never win the lane away from the hold"
        );
        // A DIFFERENT claim re-runs the verify (still no ticket → still refused),
        // proving the cache keys on the claim rather than latching the first id.
        assert!(cache.refused_at(&claim("other", None), now, SystemTime::now()));
    }
}
