//! Claiming a pairing code: the owner write and its typed errors.

use super::*;

/// The four-key object the claim writer persists, in the Python insertion order
/// (`paired`, `api_key`, `owner_id`, `paired_at`). A struct (not a `serde_json`
/// map) so the on-disk key order is the Python order regardless of the
/// `serde_json` `preserve_order` feature.
#[derive(Serialize)]
pub(super) struct ClaimedState<'a> {
    pub(super) paired: bool,
    pub(super) api_key: &'a str,
    pub(super) owner_id: &'a str,
    pub(super) paired_at: f64,
}

/// The result of a claim: the persisted API key plus the timestamp written.
pub struct ClaimOutcome {
    pub api_key: String,
}

/// Why a claim failed: minting a fresh key drew no entropy, or the persist hit
/// the filesystem. The route maps each to its own 500 message so the operator
/// (and the logs) can tell a credential-entropy failure from a disk failure.
#[derive(Debug)]
pub enum ClaimError {
    /// `getrandom` failed while minting a fresh API key — fail closed rather than
    /// emit a predictable key.
    KeyGen(getrandom::Error),
    /// Serializing or atomically writing `pairing.json` failed.
    Persist(std::io::Error),
    /// The existing document could not be read or parsed. A claim never
    /// overwrites a file it could not read: that file may hold a live pairing.
    Unreadable(String),
}

impl std::fmt::Display for ClaimError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClaimError::KeyGen(e) => write!(f, "key generation failed: {e}"),
            ClaimError::Persist(e) => write!(f, "{e}"),
            ClaimError::Unreadable(e) => write!(f, "pairing state unreadable: {e}"),
        }
    }
}

impl std::error::Error for ClaimError {}

/// Claim the agent for `user_id`, mirroring `PairingManager.claim`. Reads the
/// current document, prefers the cached `pending_api_key` over a fresh key, then
/// atomically writes the four-key paired object (dropping the code + pending key
/// the same way the Python writer does). Returns the persisted API key.
///
/// Fails closed on a `getrandom` error when no pending key is cached: a fresh
/// claim never emits a predictable/all-zero key. Fails closed as well when the
/// existing document cannot be read or parsed.
///
/// `now` is the `paired_at` timestamp (unix seconds, fractional), passed in so a
/// test can pin it; production passes the wall clock.
pub fn claim(path: &Path, user_id: &str, now: f64) -> Result<ClaimOutcome, ClaimError> {
    let current = PairingDoc::read(path).map_err(ClaimError::Unreadable)?;
    // Idempotent re-claim: an already-paired agent returns its EXISTING key
    // rather than rotating to the pending one. Rotating on re-claim silently
    // invalidates every client already holding the key (the GCS, other
    // browsers), so a key change must be an explicit unpair-then-claim. This
    // also lets a first-party surface (the agent's own webapp, reached over the
    // LAN) re-acquire the current key without disturbing existing pairings.
    if current.paired {
        if let Some(existing) = current.api_key.clone().filter(|k| !k.is_empty()) {
            return Ok(ClaimOutcome { api_key: existing });
        }
    }
    let api_key = match current.pending_api_key.clone().filter(|k| !k.is_empty()) {
        Some(pending) => pending,
        None => generate_api_key().map_err(ClaimError::KeyGen)?,
    };
    write_claimed(path, user_id, &api_key, now)?;
    Ok(ClaimOutcome { api_key })
}

/// Claim the agent for `user_id` under `api_key`, a key the caller already
/// handed to the pairing backend (the external-code accept registers the key
/// with the cloud before the device persists it). Mirrors
/// `PairingManager.claim(user_id, api_key)`: the same four-key paired object,
/// with the code and the pending key dropped.
///
/// The caller holds [`lock_writers`] and has checked the node is unpaired. The
/// document is still read first so a file that cannot be read is never
/// overwritten: it may hold a live pairing.
pub fn claim_with_key(
    path: &Path,
    user_id: &str,
    api_key: &str,
    now: f64,
) -> Result<(), ClaimError> {
    PairingDoc::read(path).map_err(ClaimError::Unreadable)?;
    write_claimed(path, user_id, api_key, now)
}

/// Atomically write the paired object.
pub(super) fn write_claimed(
    path: &Path,
    user_id: &str,
    api_key: &str,
    now: f64,
) -> Result<(), ClaimError> {
    let state = ClaimedState {
        paired: true,
        api_key,
        owner_id: user_id,
        paired_at: now,
    };
    let body = serde_json::to_vec_pretty(&state)
        .map_err(|e| ClaimError::Persist(std::io::Error::other(e.to_string())))?;
    atomic_write_0600(path, &body).map_err(ClaimError::Persist)
}
