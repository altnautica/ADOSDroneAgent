//! The dashboard-access PIN record on disk (`/etc/ados/dashboard-pin.json`).
//!
//! A paired agent's own web dashboard is reached from off-box without the
//! `X-ADOS-Key` (that key lives in the GCS). Rather than prompt for the raw key,
//! the dashboard offers a short numeric PIN: the first visitor on the LAN sets
//! one (trust-on-first-use, the same stance the pairing-claim flow takes), and a
//! returning visitor enters it. A correct PIN mints a
//! [`ados_protocol::dashboard_session`] token the front accepts as an
//! alternative data-plane credential.
//!
//! ## Threat model — read before touching the hash
//!
//! A 4-digit PIN has only 10⁴ combinations, so the stored hash is NOT a
//! meaningful barrier against an attacker who already has the file: any hash of a
//! 4-digit input falls in milliseconds. That is fine, because the PIN is a
//! **convenience gate on a trusted LAN**, not a cryptographic secret. The real
//! defenses are:
//!
//! 1. the file is `0600`, owner-only (an attacker with it already has root);
//! 2. physical presence on the LAN is the reach boundary (same as pairing-claim);
//! 3. the verify lockout ladder makes online brute-force infeasible.
//!
//! So the hash is a salted SHA-256 (plenty to avoid storing the PIN in the
//! clear + to salt across nodes), not a slow KDF — a slow KDF would buy nothing
//! against 10⁴ inputs and only slow the honest verify path.
//!
//! The salt does one load-bearing job beyond hashing: it is folded into the
//! session-token key (see [`ados_protocol::dashboard_session`]), so writing a
//! fresh salt on every set/reset revokes every previously-minted session.

use std::path::{Path, PathBuf};

use ados_protocol::dashboard_session::{
    now_unix, DashboardSession, DashboardSessionIssuer, DEFAULT_TTL_SECONDS,
};
use ados_protocol::pairing_posture::{constant_time_eq, Pairing};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Canonical dashboard-PIN record path. Overridable via `ADOS_DASHBOARD_PIN_JSON`
/// for tests, mirroring the sibling `ADOS_PAIRING_JSON` override convention.
pub const DEFAULT_DASHBOARD_PIN_PATH: &str = "/etc/ados/dashboard-pin.json";

/// The header a dashboard browser sends its session token on. Accepted by the
/// front's auth edge as an alternative to `X-ADOS-Key`.
pub const DASHBOARD_SESSION_HEADER: &str = "x-ados-dashboard-session";

/// The salt length in bytes (128-bit, ample to separate nodes + key the session).
const SALT_LEN: usize = 16;

/// PIN length bounds. The dashboard uses a 4-digit PIN; the store accepts 4–12
/// digits so a longer PIN is not rejected, but nothing shorter than 4.
const MIN_PIN_LEN: usize = 4;
const MAX_PIN_LEN: usize = 12;

/// Wrong attempts before the first lockout window. Also the count the "enter"
/// splash counts down from.
const LOCK_AFTER: u32 = 5;

/// The persisted PIN record. Absent file = no PIN set.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DashboardPinDoc {
    /// Hex `SHA-256(salt || pin_utf8)`.
    pub pin_hash: String,
    /// Hex-encoded random salt. Folded into the session key so a reset revokes
    /// live sessions.
    pub salt: String,
    /// Unix seconds (fractional) the PIN was last set.
    #[serde(default)]
    pub set_at: f64,
    /// Consecutive wrong verify attempts since the last success.
    #[serde(default)]
    pub failed_attempts: u32,
    /// Unix seconds (fractional) the lockout expires; `0.0` when not locked.
    #[serde(default)]
    pub locked_until: f64,
}

/// The public status the `pin/status` route reports.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PinStatus {
    pub pin_set: bool,
    pub locked: bool,
    /// The lockout expiry (unix seconds), meaningful only when `locked`.
    pub locked_until: f64,
}

/// The outcome of a verify attempt.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VerifyOutcome {
    /// Correct PIN: the caller may mint a session.
    Ok,
    /// Wrong PIN, not (yet) locked. `remaining_attempts` counts down to the lock.
    Wrong { remaining_attempts: u32 },
    /// Locked out until `locked_until` (unix seconds).
    Locked { locked_until: f64 },
    /// No PIN is set, so there is nothing to verify against.
    NotSet,
}

/// Why a set failed.
#[derive(Debug)]
pub enum PinError {
    /// The PIN is not 4–12 ASCII digits.
    InvalidPin,
    /// `getrandom` failed while minting the salt — fail closed rather than use a
    /// predictable salt (which would weaken the session key).
    SaltGen(getrandom::Error),
    /// Serializing or atomically writing the record failed.
    Persist(std::io::Error),
}

impl std::fmt::Display for PinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PinError::InvalidPin => write!(f, "PIN must be 4 to 12 digits"),
            PinError::SaltGen(e) => write!(f, "salt generation failed: {e}"),
            PinError::Persist(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for PinError {}

/// The dashboard-PIN store: a path plus read/write operations. Every op reads or
/// writes the file fresh (the record is tiny and ops are infrequent), so there is
/// no cache to keep coherent across the two holders (the routes + the auth edge).
#[derive(Debug, Clone)]
pub struct DashboardPin {
    path: PathBuf,
}

impl DashboardPin {
    /// Build a store against the standard path (honouring `ADOS_DASHBOARD_PIN_JSON`).
    pub fn new() -> Self {
        let path = std::env::var("ADOS_DASHBOARD_PIN_JSON")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_DASHBOARD_PIN_PATH));
        Self { path }
    }

    /// Build a store against an explicit path (tests + the daemon's injectable path).
    pub fn with_path(path: PathBuf) -> Self {
        Self { path }
    }

    /// The record path this store reads + writes.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn load(&self) -> Option<DashboardPinDoc> {
        let text = std::fs::read_to_string(&self.path).ok()?;
        let doc: DashboardPinDoc = serde_json::from_str(&text).ok()?;
        // A record with no hash/salt is treated as unset (a truncated / cleared
        // file that somehow parsed).
        if doc.pin_hash.is_empty() || doc.salt.is_empty() {
            None
        } else {
            Some(doc)
        }
    }

    /// Whether a PIN is set.
    pub fn is_set(&self) -> bool {
        self.load().is_some()
    }

    /// The current status for the `pin/status` route.
    pub fn status(&self, now: f64) -> PinStatus {
        match self.load() {
            Some(doc) => PinStatus {
                pin_set: true,
                locked: doc.locked_until > now,
                locked_until: doc.locked_until,
            },
            None => PinStatus {
                pin_set: false,
                locked: false,
                locked_until: 0.0,
            },
        }
    }

    /// The decoded salt, or `None` when no PIN is set / the stored salt is not hex.
    pub fn salt(&self) -> Option<Vec<u8>> {
        let doc = self.load()?;
        hex::decode(&doc.salt).ok()
    }

    /// Set (or replace) the PIN. Mints a fresh salt, hashes, and atomically
    /// writes the record with the lockout counters cleared. `now` is the `set_at`
    /// stamp (unix seconds). Validates the PIN is 4–12 digits.
    pub fn set_pin(&self, pin: &str, now: f64) -> Result<(), PinError> {
        if !is_valid_pin(pin) {
            return Err(PinError::InvalidPin);
        }
        let mut salt = [0u8; SALT_LEN];
        getrandom::getrandom(&mut salt).map_err(PinError::SaltGen)?;
        let doc = DashboardPinDoc {
            pin_hash: hash_pin(&salt, pin),
            salt: hex::encode(salt),
            set_at: now,
            failed_attempts: 0,
            locked_until: 0.0,
        };
        self.persist(&doc).map_err(PinError::Persist)
    }

    /// Verify `pin`. On a correct PIN the lockout counters are reset and `Ok` is
    /// returned. On a wrong PIN the failed-attempt counter advances and, past the
    /// threshold, a lockout window is set. While locked, a verify does not consume
    /// an attempt — it returns `Locked` immediately. `now` is the wall clock.
    pub fn verify_pin(&self, pin: &str, now: f64) -> VerifyOutcome {
        let Some(mut doc) = self.load() else {
            return VerifyOutcome::NotSet;
        };
        if doc.locked_until > now {
            return VerifyOutcome::Locked {
                locked_until: doc.locked_until,
            };
        }
        let salt = match hex::decode(&doc.salt) {
            Ok(s) => s,
            // A corrupt salt cannot be verified against; treat as not-set rather
            // than silently accept.
            Err(_) => return VerifyOutcome::NotSet,
        };
        let candidate = hash_pin(&salt, pin);
        if constant_time_eq(candidate.as_bytes(), doc.pin_hash.as_bytes()) {
            // Reset the counters on success (best-effort persist; a write failure
            // does not deny the correct PIN).
            if doc.failed_attempts != 0 || doc.locked_until != 0.0 {
                doc.failed_attempts = 0;
                doc.locked_until = 0.0;
                let _ = self.persist(&doc);
            }
            return VerifyOutcome::Ok;
        }
        // Wrong: advance the counter, arm the ladder, persist.
        doc.failed_attempts = doc.failed_attempts.saturating_add(1);
        let lock = lock_seconds(doc.failed_attempts);
        if lock > 0.0 {
            doc.locked_until = now + lock;
        }
        let _ = self.persist(&doc);
        if doc.locked_until > now {
            VerifyOutcome::Locked {
                locked_until: doc.locked_until,
            }
        } else {
            VerifyOutcome::Wrong {
                remaining_attempts: LOCK_AFTER.saturating_sub(doc.failed_attempts),
            }
        }
    }

    /// Clear the PIN (remove the record). A subsequent visit re-enters the
    /// trust-on-first-use "set a PIN" flow, and the salt rotation revokes every
    /// live session. Absent file is a no-op success.
    pub fn clear(&self) -> std::io::Result<()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Mint a dashboard session under the given pairing key + the stored salt.
    /// `None` when no PIN is set (no salt to key with). Called right after a
    /// successful set/verify, so the salt is present.
    pub fn mint_session(&self, api_key: &str) -> Option<DashboardSession> {
        let salt = self.salt()?;
        Some(
            DashboardSessionIssuer::from_api_key_and_salt(api_key, &salt).mint(DEFAULT_TTL_SECONDS),
        )
    }

    /// Whether `token` is a valid dashboard session for the current pairing key +
    /// salt. `false` when unpaired (the data plane is open anyway, so the edge
    /// never consults this) or when no PIN is set. This is the alternative
    /// data-plane credential the front's auth edge accepts alongside `X-ADOS-Key`.
    pub fn session_valid_for(&self, pairing: &Pairing, token: &str) -> bool {
        let Pairing::Paired(key) = pairing else {
            return false;
        };
        let Some(salt) = self.salt() else {
            return false;
        };
        DashboardSessionIssuer::from_api_key_and_salt(key, &salt)
            .verify(token, now_unix())
            .is_ok()
    }

    fn persist(&self, doc: &DashboardPinDoc) -> std::io::Result<()> {
        let body =
            serde_json::to_vec_pretty(doc).map_err(|e| std::io::Error::other(e.to_string()))?;
        crate::pairing_store::atomic_write_0600(&self.path, &body)
    }
}

impl Default for DashboardPin {
    fn default() -> Self {
        Self::new()
    }
}

/// Hex `SHA-256(salt || pin)`. See the module threat-model note on why a plain
/// salted SHA-256 (not a slow KDF) is the right choice for a 4-digit PIN.
fn hash_pin(salt: &[u8], pin: &str) -> String {
    let mut h = Sha256::new();
    h.update(salt);
    h.update(pin.as_bytes());
    hex::encode(h.finalize())
}

/// A PIN is 4–12 ASCII digits.
fn is_valid_pin(pin: &str) -> bool {
    (MIN_PIN_LEN..=MAX_PIN_LEN).contains(&pin.len()) && pin.bytes().all(|b| b.is_ascii_digit())
}

/// The lockout window (seconds) for a given consecutive-wrong count: 0 below the
/// threshold, then an escalating ladder. Re-armed on each wrong attempt past the
/// threshold, so 5–9 wrong throttle at 30 s, 10–14 at 5 min, 15+ at 30 min —
/// enough to make an online 10⁴-space brute force infeasible.
fn lock_seconds(failed: u32) -> f64 {
    if failed >= 15 {
        1800.0
    } else if failed >= 10 {
        300.0
    } else if failed >= LOCK_AFTER {
        30.0
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(dir: &Path) -> DashboardPin {
        DashboardPin::with_path(dir.join("dashboard-pin.json"))
    }

    #[test]
    fn absent_file_is_unset() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        assert!(!s.is_set());
        assert_eq!(
            s.status(100.0),
            PinStatus {
                pin_set: false,
                locked: false,
                locked_until: 0.0
            }
        );
        assert_eq!(s.verify_pin("1234", 100.0), VerifyOutcome::NotSet);
        assert!(s.salt().is_none());
    }

    #[test]
    fn set_then_verify_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.set_pin("1234", 10.0).unwrap();
        assert!(s.is_set());
        assert_eq!(s.verify_pin("1234", 11.0), VerifyOutcome::Ok);
        // A wrong PIN is Wrong with the remaining countdown, not Ok.
        assert_eq!(
            s.verify_pin("0000", 12.0),
            VerifyOutcome::Wrong {
                remaining_attempts: LOCK_AFTER - 1
            }
        );
        // A subsequent correct PIN resets the counter.
        assert_eq!(s.verify_pin("1234", 13.0), VerifyOutcome::Ok);
        assert_eq!(
            s.verify_pin("0000", 14.0),
            VerifyOutcome::Wrong {
                remaining_attempts: LOCK_AFTER - 1
            }
        );
    }

    #[test]
    fn rejects_non_digit_and_short_pins() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        assert!(matches!(s.set_pin("12", 0.0), Err(PinError::InvalidPin)));
        assert!(matches!(s.set_pin("12ab", 0.0), Err(PinError::InvalidPin)));
        assert!(matches!(s.set_pin("", 0.0), Err(PinError::InvalidPin)));
        assert!(s.set_pin("1234", 0.0).is_ok());
        assert!(s.set_pin("123456", 0.0).is_ok());
    }

    #[test]
    fn lockout_after_five_wrong() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.set_pin("4321", 0.0).unwrap();
        // Four wrong: still counting down, not locked.
        for i in 1..=4u32 {
            assert_eq!(
                s.verify_pin("0000", i as f64),
                VerifyOutcome::Wrong {
                    remaining_attempts: LOCK_AFTER - i
                }
            );
        }
        // Fifth wrong: locked for 30 s.
        match s.verify_pin("0000", 5.0) {
            VerifyOutcome::Locked { locked_until } => assert_eq!(locked_until, 35.0),
            other => panic!("expected Locked, got {other:?}"),
        }
        // While locked, even the CORRECT PIN is refused (returns Locked, no
        // attempt consumed).
        match s.verify_pin("4321", 10.0) {
            VerifyOutcome::Locked { locked_until } => assert_eq!(locked_until, 35.0),
            other => panic!("expected Locked, got {other:?}"),
        }
        // After the window, the correct PIN unlocks + resets.
        assert_eq!(s.verify_pin("4321", 40.0), VerifyOutcome::Ok);
        assert_eq!(
            s.verify_pin("0000", 41.0),
            VerifyOutcome::Wrong {
                remaining_attempts: LOCK_AFTER - 1
            }
        );
    }

    #[test]
    fn clear_removes_and_re_tofus() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.set_pin("1234", 0.0).unwrap();
        assert!(s.is_set());
        s.clear().unwrap();
        assert!(!s.is_set());
        // Clearing an absent file is a no-op success.
        s.clear().unwrap();
    }

    #[test]
    fn set_rotates_the_salt() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.set_pin("1234", 0.0).unwrap();
        let salt1 = s.salt().unwrap();
        s.set_pin("1234", 1.0).unwrap();
        let salt2 = s.salt().unwrap();
        assert_ne!(salt1, salt2, "each set mints a fresh salt");
    }

    #[test]
    fn session_round_trip_and_reset_revocation() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.set_pin("1234", 0.0).unwrap();
        let paired = Pairing::Paired("ados_key".to_string());
        let sess = s
            .mint_session("ados_key")
            .expect("a set PIN mints a session");
        assert!(
            s.session_valid_for(&paired, &sess.token),
            "a fresh session verifies"
        );
        // A reset (new salt) revokes the live session.
        s.set_pin("5678", 1.0).unwrap();
        assert!(
            !s.session_valid_for(&paired, &sess.token),
            "resetting the PIN revokes the prior session"
        );
        // Unpaired never validates a session (the data plane is open anyway).
        assert!(!s.session_valid_for(&Pairing::Unpaired, &sess.token));
        // A wrong pairing key does not validate.
        let other = Pairing::Paired("different".to_string());
        let fresh = s.mint_session("ados_key").unwrap();
        assert!(!s.session_valid_for(&other, &fresh.token));
    }

    #[cfg(unix)]
    #[test]
    fn record_is_0600() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.set_pin("1234", 0.0).unwrap();
        let mode = std::fs::metadata(s.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "dashboard-pin.json must be 0600");
    }
}
