//! Who the programmatic channel injector actually is.
//!
//! The hybrid authority rule hands the transmit lane to the programmatic
//! injector when the injector is the same client that holds the pilot-in-command
//! claim. That comparison needs an identity, and the identity used to be a
//! string the caller put in its own request body.
//!
//! Which meant the check compared a self-asserted name against a name any
//! authenticated caller can read back off the claim routes. Anything that could
//! open the command socket could therefore name the current human holder and
//! take the lane from them: not by defeating the arbiter, but by answering its
//! question with the answer it was looking for.
//!
//! So the name has to be attested by something the caller cannot mint. This
//! reuses the agent's shipped self-contained ticket: an HMAC over a scope
//! string, keyed by a value derived from the pairing key, verifiable by any
//! daemon that can read `pairing.json` and forgeable by nothing that cannot.
//! Binding the client id INTO the scope is what makes it an identity rather
//! than a permission — a ticket minted for one client will not verify for
//! another.
//!
//! An unpaired node has no key, so there is nothing to mint or verify against
//! and no credential anywhere else on the node either; it keeps the previous
//! behaviour, matching the posture every other native surface takes while a
//! node is unclaimed. A paired node with no ticket produces NO verified
//! identity, which the authority rule already handles as the safe outcome: the
//! injector does not hold the claim, so the human input path keeps the lane.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ados_protocol::pairing_posture::{load_pairing, Pairing};
use ados_protocol::ws_ticket::{crsf_inject_scope, mint_scoped_ticket, now_unix, WsTicketIssuer};

/// How long a loaded pairing state is trusted before `pairing.json` is re-read.
/// Matches the MAVLink proxy's own cache window; a re-pair takes effect within
/// it rather than needing a restart.
const PAIRING_TTL: Duration = Duration::from_secs(5);

/// Default location of the agent's pairing state, overridable by the same
/// environment variable the HTTP control surface and the MAVLink proxy read.
fn default_pairing_path() -> PathBuf {
    std::env::var("ADOS_PAIRING_JSON")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/etc/ados/pairing.json"))
}

/// Resolves the injector identity a command may claim.
pub struct InjectorAuth {
    pairing_path: PathBuf,
    cached: std::sync::Mutex<Option<(Pairing, std::time::Instant)>>,
}

impl InjectorAuth {
    pub fn new(pairing_path: PathBuf) -> Self {
        Self {
            pairing_path,
            cached: std::sync::Mutex::new(None),
        }
    }

    /// Read the pairing state from the conventional location.
    pub fn from_env() -> Self {
        Self::new(default_pairing_path())
    }

    fn pairing(&self) -> Pairing {
        let mut guard = self.cached.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((p, at)) = guard.as_ref() {
            if at.elapsed() < PAIRING_TTL {
                return p.clone();
            }
        }
        let fresh = load_pairing(&self.pairing_path);
        *guard = Some((fresh.clone(), std::time::Instant::now()));
        fresh
    }

    /// Resolve the identity an injection may be credited to.
    ///
    /// `asserted` is the caller's own claim and is never the answer on a paired
    /// node; `ticket` is what makes it one. `None` means no identity was
    /// established, which is not an error — the injection still applies, it just
    /// cannot win the lane away from a human holder.
    pub fn verify(&self, asserted: Option<&str>, ticket: Option<&str>) -> Option<String> {
        let asserted = asserted?;
        match self.pairing() {
            // Nothing on the node can mint or check a ticket yet, and every
            // other surface is open in this state. Refusing here would break
            // the lane on a node that is not yet claimed without protecting
            // anything, because there is no credential to protect it with.
            Pairing::Unpaired => Some(asserted.to_string()),
            Pairing::Paired(api_key) => {
                let ticket = ticket?;
                let issuer = WsTicketIssuer::from_api_key(&api_key);
                issuer
                    .verify(ticket, &crsf_inject_scope(asserted), now_unix())
                    .ok()
                    .map(|()| asserted.to_string())
            }
        }
    }
}

/// Mint an injector attestation for `client_id` against the pairing key at
/// `path`. The mirror of [`InjectorAuth::verify`], kept beside it so the two
/// halves cannot drift on the scope; `None` on an unpaired node, where the
/// verifier does not ask for one.
pub fn mint_injector_ticket(path: &Path, client_id: &str, ttl_seconds: i64) -> Option<String> {
    mint_scoped_ticket(path, &crsf_inject_scope(client_id), ttl_seconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paired(dir: &Path, key: &str) -> PathBuf {
        let path = dir.join("pairing.json");
        std::fs::write(&path, format!(r#"{{"paired": true, "api_key": "{key}"}}"#)).unwrap();
        path
    }

    #[test]
    fn a_paired_node_will_not_take_a_bare_name() {
        // The whole defect: the name alone used to be the credential, and the
        // name of the current holder is readable by any API caller.
        let dir = tempfile::tempdir().unwrap();
        let auth = InjectorAuth::new(paired(dir.path(), "k"));
        assert_eq!(auth.verify(Some("operator-a"), None), None);
    }

    #[test]
    fn a_ticket_attests_the_name_it_was_minted_for() {
        let dir = tempfile::tempdir().unwrap();
        let path = paired(dir.path(), "k");
        let auth = InjectorAuth::new(path.clone());
        let ticket = mint_injector_ticket(&path, "autopilot-1", 60).unwrap();
        assert_eq!(
            auth.verify(Some("autopilot-1"), Some(&ticket)),
            Some("autopilot-1".to_string())
        );
    }

    #[test]
    fn a_ticket_does_not_carry_over_to_another_name() {
        // Replaying someone else's ticket under the holder's name is the same
        // attack in a different shape, so the id is signed material.
        let dir = tempfile::tempdir().unwrap();
        let path = paired(dir.path(), "k");
        let auth = InjectorAuth::new(path.clone());
        let ticket = mint_injector_ticket(&path, "autopilot-1", 60).unwrap();
        assert_eq!(auth.verify(Some("operator-a"), Some(&ticket)), None);
    }

    #[test]
    fn a_ticket_from_a_different_key_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let auth = InjectorAuth::new(paired(dir.path(), "real-key"));
        let forged =
            mint_injector_ticket(&paired(other.path(), "guessed-key"), "operator-a", 60).unwrap();
        assert_eq!(auth.verify(Some("operator-a"), Some(&forged)), None);
    }

    #[test]
    fn an_unpaired_node_keeps_working_without_a_ticket() {
        let dir = tempfile::tempdir().unwrap();
        let auth = InjectorAuth::new(dir.path().join("absent.json"));
        assert_eq!(
            auth.verify(Some("autopilot-1"), None),
            Some("autopilot-1".to_string())
        );
        assert_eq!(
            mint_injector_ticket(&dir.path().join("absent.json"), "x", 60),
            None
        );
    }

    #[test]
    fn no_name_is_no_identity() {
        let dir = tempfile::tempdir().unwrap();
        let auth = InjectorAuth::new(paired(dir.path(), "k"));
        assert_eq!(auth.verify(None, None), None);
    }
}
