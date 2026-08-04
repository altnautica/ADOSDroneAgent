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
use std::time::SystemTime;

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

/// The live gate: read the pairing + PIC state off disk and decide. Reads the
/// PIC view FRESH each call (it is the live authority; a cached "unclaimed"
/// could outlive the operator's grab), staleness-gated by the sidecar's mtime.
pub fn injector_refused(pairing_path: &Path, pic_state_path: &Path, claim: &InjectorClaim) -> bool {
    let verified = verify_injector(pairing_path, claim);
    let pic = read_pic_view(pic_state_path, SystemTime::now());
    injector_refused_decision(pic.as_ref(), verified.as_deref())
}

/// The pairing file, respecting the same `ADOS_PAIRING_JSON` override every
/// other surface reads.
fn default_pairing_path() -> PathBuf {
    std::env::var("ADOS_PAIRING_JSON")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/etc/ados/pairing.json"))
}

/// [`injector_refused`] against the canonical on-box paths (the PIC sidecar in
/// the run dir, the pairing file). This is what the FC-write path calls; it runs
/// only when a command actually carries an injector claim, which nothing does
/// until a producer is armed, so it is inert by default.
pub fn injector_refused_default(claim: &InjectorClaim) -> bool {
    injector_refused(
        &default_pairing_path(),
        &ados_hid::paths::pic_state_json(),
        claim,
    )
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
}
