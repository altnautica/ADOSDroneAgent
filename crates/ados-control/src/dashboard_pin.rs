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
//! 3. the verify lockout ladder makes online brute-force infeasible: per caller,
//!    plus a node-wide budget of [`NODE_FAILURE_BUDGET`] failures per rolling
//!    hour across every caller, so spraying from many addresses buys nothing.
//!
//! So the hash is a salted SHA-256 (plenty to avoid storing the PIN in the
//! clear + to salt across nodes), not a slow KDF — a slow KDF would buy nothing
//! against 10⁴ inputs and only slow the honest verify path.
//!
//! The salt does one load-bearing job beyond hashing: it is folded into the
//! session-token key (see [`ados_protocol::dashboard_session`]), so writing a
//! fresh salt on every set/reset revokes every previously-minted session.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use ados_protocol::dashboard_session::{
    now_unix, DashboardSession, DashboardSessionIssuer, DEFAULT_TTL_SECONDS,
};
use ados_protocol::pairing_posture::{constant_time_eq, Pairing};

pub use crate::auth::PeerKey;
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

/// How long a loaded record is trusted before the file is stat-ed again. The
/// same short TTL the pairing reader uses: the edge validates a dashboard
/// session on every browser request, and each validation needs the salt.
const DOC_TTL: Duration = Duration::from_secs(2);

/// Past this many tracked callers, entries that are not currently locked are
/// dropped, so a caller spraying addresses cannot grow the table without bound.
const MAX_TRACKED_PEERS: usize = 1024;

/// Wrong PINs, across every caller, that lock the whole node.
pub const NODE_FAILURE_BUDGET: usize = 30;

/// The rolling window the node-wide budget counts over (seconds).
const NODE_WINDOW_S: f64 = 3600.0;

/// How long the node-wide lock holds unless cleared (seconds).
const NODE_LOCK_S: f64 = 1800.0;

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
}

/// One caller's consecutive-wrong count and lockout expiry.
#[derive(Debug, Clone, Copy, Default)]
struct Attempts {
    failed: u32,
    locked_until: f64,
}

/// The cached record, keyed on the file's size and modification time.
#[derive(Debug)]
struct DocCache {
    doc: Option<DashboardPinDoc>,
    stamp: Option<(SystemTime, u64)>,
    at: Instant,
}

/// The node-wide failure budget: recent failure times and the lock expiry.
#[derive(Debug, Default)]
struct NodeBudget {
    failures: VecDeque<f64>,
    locked_until: f64,
}

impl NodeBudget {
    /// Count one wrong PIN at `now`. Returns the lock expiry when this failure
    /// exhausts the budget.
    fn record_failure(&mut self, now: f64) -> Option<f64> {
        while self
            .failures
            .front()
            .is_some_and(|t| now - *t >= NODE_WINDOW_S)
        {
            self.failures.pop_front();
        }
        self.failures.push_back(now);
        if self.failures.len() < NODE_FAILURE_BUDGET {
            return None;
        }
        self.failures.clear();
        self.locked_until = now + NODE_LOCK_S;
        Some(self.locked_until)
    }
}

/// The public status the `pin/status` route reports, for one caller.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PinStatus {
    pub pin_set: bool,
    pub locked: bool,
    /// The lockout expiry (unix seconds), meaningful only when `locked`.
    pub locked_until: f64,
    /// The lock is node-wide (every caller), not this caller's own.
    pub node_locked: bool,
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
    /// Every caller is locked out until `locked_until` (unix seconds): the
    /// node-wide budget is spent. `engaged` is true on the failure that spent it.
    NodeLocked { locked_until: f64, engaged: bool },
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

/// The dashboard-PIN store: the record path, the per-caller lockout ladder, the
/// node-wide failure budget and a short-lived record cache. Every holder shares
/// one instance (behind an `Arc`), so a set or clear through the routes
/// invalidates the cache the auth edge reads.
///
/// The lockout ladder is per caller and in memory. A single ladder in the record
/// let any host on the network lock the operator out indefinitely by sending one
/// wrong PIN each time the lock expired. The ladder's mutex also serialises the
/// whole verify, so two concurrent wrong attempts both count.
#[derive(Debug, Clone)]
pub struct DashboardPin {
    path: PathBuf,
    attempts: Arc<Mutex<HashMap<PeerKey, Attempts>>>,
    node: Arc<parking_lot::Mutex<NodeBudget>>,
    doc_cache: Arc<parking_lot::Mutex<Option<DocCache>>>,
}

impl DashboardPin {
    /// Build a store against the standard path (honouring `ADOS_DASHBOARD_PIN_JSON`).
    pub fn new() -> Self {
        let path = std::env::var("ADOS_DASHBOARD_PIN_JSON")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_DASHBOARD_PIN_PATH));
        Self::with_path(path)
    }

    /// Build a store against an explicit path (tests + the daemon's injectable path).
    pub fn with_path(path: PathBuf) -> Self {
        Self {
            path,
            attempts: Arc::new(Mutex::new(HashMap::new())),
            node: Arc::new(parking_lot::Mutex::new(NodeBudget::default())),
            doc_cache: Arc::new(parking_lot::Mutex::new(None)),
        }
    }

    /// The record path this store reads + writes.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The record, read at most once per [`DOC_TTL`] and parsed again only
    /// when the file's size or modification time changed.
    fn load(&self) -> Option<DashboardPinDoc> {
        let mut cache = self.doc_cache.lock();
        if let Some(c) = cache.as_ref() {
            if c.at.elapsed() < DOC_TTL {
                return c.doc.clone();
            }
        }
        let stamp = std::fs::metadata(&self.path)
            .ok()
            .and_then(|m| Some((m.modified().ok()?, m.len())));
        if let Some(c) = cache.as_mut() {
            if c.stamp == stamp {
                c.at = Instant::now();
                return c.doc.clone();
            }
        }
        let doc = read_doc(&self.path);
        *cache = Some(DocCache {
            doc: doc.clone(),
            stamp,
            at: Instant::now(),
        });
        doc
    }

    /// Whether a PIN is set.
    pub fn is_set(&self) -> bool {
        self.load().is_some()
    }

    /// The current status for the `pin/status` route, as seen by `peer`: the
    /// node-wide lock when it holds, else that caller's own lock.
    pub fn status(&self, peer: PeerKey, now: f64) -> PinStatus {
        if self.load().is_none() {
            return PinStatus {
                pin_set: false,
                locked: false,
                locked_until: 0.0,
                node_locked: false,
            };
        }
        let attempts = self.attempts.lock().unwrap_or_else(|p| p.into_inner());
        let node_until = self.node.lock().locked_until;
        if node_until > now {
            return PinStatus {
                pin_set: true,
                locked: true,
                locked_until: node_until,
                node_locked: true,
            };
        }
        let locked_until = attempts.get(&peer).map_or(0.0, |a| a.locked_until);
        PinStatus {
            pin_set: true,
            locked: locked_until > now,
            locked_until,
            node_locked: false,
        }
    }

    /// The decoded salt, or `None` when no PIN is set / the stored salt is not
    /// hex. Served from the record cache; a set or clear through this store
    /// drops the cached record at once.
    pub fn salt(&self) -> Option<Vec<u8>> {
        self.load().and_then(|doc| hex::decode(&doc.salt).ok())
    }

    fn invalidate_doc(&self) {
        *self.doc_cache.lock() = None;
    }

    /// Set (or replace) the PIN. Mints a fresh salt, hashes, atomically writes
    /// the record and clears every caller's lockout and the node-wide lock.
    /// `now` is the `set_at` stamp (unix seconds). Validates the PIN is 4–12
    /// digits.
    pub fn set_pin(&self, pin: &str, now: f64) -> Result<(), PinError> {
        if !is_valid_pin(pin) {
            return Err(PinError::InvalidPin);
        }
        let mut salt = [0u8; SALT_LEN];
        getrandom::fill(&mut salt).map_err(PinError::SaltGen)?;
        let doc = DashboardPinDoc {
            pin_hash: hash_pin(&salt, pin),
            salt: hex::encode(salt),
            set_at: now,
        };
        let mut attempts = self.attempts.lock().unwrap_or_else(|p| p.into_inner());
        let written = self.persist(&doc).map_err(PinError::Persist);
        self.invalidate_doc();
        if written.is_ok() {
            attempts.clear();
            *self.node.lock() = NodeBudget::default();
        }
        written
    }

    /// Verify `pin` for the caller `peer`. On a correct PIN that caller's ladder
    /// resets and `Ok` is returned. On a wrong PIN its counter advances and, past
    /// the threshold, a lockout window is set; the failure also counts against
    /// the node-wide budget, which once spent locks every caller. While either
    /// lock holds, a verify does not consume an attempt — it returns at once,
    /// even for the correct PIN. `now` is the wall clock. Serialised: the
    /// ladder's mutex is held for the whole attempt.
    pub fn verify_pin(&self, peer: PeerKey, pin: &str, now: f64) -> VerifyOutcome {
        let mut attempts = self.attempts.lock().unwrap_or_else(|p| p.into_inner());
        let Some(doc) = self.load() else {
            return VerifyOutcome::NotSet;
        };
        let mut node = self.node.lock();
        if node.locked_until > now {
            return VerifyOutcome::NodeLocked {
                locked_until: node.locked_until,
                engaged: false,
            };
        }
        let current = attempts.get(&peer).copied().unwrap_or_default();
        if current.locked_until > now {
            return VerifyOutcome::Locked {
                locked_until: current.locked_until,
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
            attempts.remove(&peer);
            return VerifyOutcome::Ok;
        }
        if attempts.len() >= MAX_TRACKED_PEERS {
            attempts.retain(|_, a| a.locked_until > now);
        }
        let entry = attempts.entry(peer).or_default();
        entry.failed = entry.failed.saturating_add(1);
        let lock = lock_seconds(entry.failed);
        if let Some(locked_until) = node.record_failure(now) {
            return VerifyOutcome::NodeLocked {
                locked_until,
                engaged: true,
            };
        }
        if lock > 0.0 {
            entry.locked_until = now + lock;
            VerifyOutcome::Locked {
                locked_until: entry.locked_until,
            }
        } else {
            VerifyOutcome::Wrong {
                remaining_attempts: LOCK_AFTER.saturating_sub(entry.failed),
            }
        }
    }

    /// Whether `token` is a valid dashboard session for the node's CURRENT
    /// pairing state, whichever that is.
    ///
    /// The two issuers exist because an unpaired node has no pairing key to key
    /// the HMAC with, so its sessions are minted under the empty-key issuer.
    /// Two call sites used to pick one each — the unpaired edge used
    /// [`Self::session_valid_unpaired`] while the proxied path used
    /// [`Self::session_valid_for`], which returns `false` when unpaired. That
    /// disagreement was harmless only by accident: an unpaired node's data plane
    /// was open anyway, so the stricter one was never consulted. It is no longer
    /// open — the media plane is now gated in both states — so the two layers
    /// have to mean the same thing by construction rather than by luck.
    pub fn session_valid(&self, pairing: &Pairing, token: &str) -> bool {
        match pairing {
            Pairing::Paired(_) => self.session_valid_for(pairing, token),
            Pairing::Unpaired => self.session_valid_unpaired(token),
            Pairing::Unreadable => false,
        }
    }

    /// Clear the PIN (remove the record) and every lock, the node-wide one
    /// included. A subsequent visit re-enters the trust-on-first-use "set a
    /// PIN" flow, and the salt rotation revokes every live session. Absent file
    /// is a no-op success.
    pub fn clear(&self) -> std::io::Result<()> {
        let mut attempts = self.attempts.lock().unwrap_or_else(|p| p.into_inner());
        let removed = match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        };
        self.invalidate_doc();
        if removed.is_ok() {
            attempts.clear();
            *self.node.lock() = NodeBudget::default();
        }
        removed
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

    /// Whether `token` is a valid dashboard session minted while the node is
    /// UNPAIRED. When unpaired there is no pairing key on file, so the session is
    /// minted (by [`mint_session`]) under the empty-key issuer; this validates a
    /// token against that same issuer + the stored salt. This is the credential
    /// the PIN gate on an unpaired node's data routes requires a trusted-LAN
    /// browser to hold: the browser mints a session via `/api/dashboard/pin/{set,
    /// verify}` and presents it as [`DASHBOARD_SESSION_HEADER`]. `false` when no
    /// PIN is set (no salt to key with).
    pub fn session_valid_unpaired(&self, token: &str) -> bool {
        let Some(salt) = self.salt() else {
            return false;
        };
        DashboardSessionIssuer::from_api_key_and_salt("", &salt)
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

/// Read and parse the record. A missing, unreadable or unparseable file, or a
/// record with no hash/salt (a truncated / cleared file that somehow parsed),
/// is no PIN.
fn read_doc(path: &Path) -> Option<DashboardPinDoc> {
    let text = std::fs::read_to_string(path).ok()?;
    let doc: DashboardPinDoc = serde_json::from_str(&text).ok()?;
    (!doc.pin_hash.is_empty() && !doc.salt.is_empty()).then_some(doc)
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
#[path = "dashboard_pin_tests.rs"]
mod tests;
