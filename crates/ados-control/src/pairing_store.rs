//! The pairing-state document on disk (`/etc/ados/pairing.json`).
//!
//! The pairing-info route reads the cloud-pair fields out of this file, and the
//! claim/unpair handlers WRITE it. The auth gate already reads it for the
//! `paired` + `api_key` posture (see [`crate::auth`]); this module carries the
//! richer read (`owner_id`, `paired_at`, `pairing_code`) plus the claim/unpair
//! writers, mirroring `ados.core.pairing.PairingManager` exactly.
//!
//! The writers are byte-faithful to the Python `PairingManager`:
//!
//! - **claim**: on an UNPAIRED agent, prefer the cached `pending_api_key` over
//!   minting a fresh key, set `paired:true` + `api_key` + `owner_id` +
//!   `paired_at`, and DROP `pairing_code` / `code_created_at` /
//!   `pending_api_key`. The written object carries exactly four keys in the
//!   Python insertion order. On an ALREADY-PAIRED agent the claim is
//!   idempotent — it returns the live `api_key` unchanged and does not rewrite
//!   or rotate. This is a deliberate divergence from the Python `claim` (which
//!   raises "already paired"); it is safe because this is the LAN-local claim
//!   surface, and rotating on re-claim would silently lock out every client
//!   already holding the key. A key change is an explicit unpair-then-claim.
//! - **unpair**: write an empty object `{}` (the Python `unpair` resets the
//!   in-memory state to `{}` then saves).
//!
//! Both writes are atomic (temp sibling + fsync + rename) and 0600, matching the
//! Python `atomic_write_json(..., mode=0o600, indent=2)`.

use std::io::Write as _;
use std::path::Path;

use serde::Serialize;

/// Canonical pairing-state path. Mirrors `PAIRING_JSON`. The crate's auth reader
/// resolves the same file (its own `DEFAULT_PAIRING_PATH`); the daemon passes the
/// resolved path to both so they agree.
pub const PAIRING_JSON: &str = "/etc/ados/pairing.json";

/// The human-friendly pairing-code charset, matching the Python `SAFE_CHARSET`:
/// no ambiguous characters (`0/O/1/I/L` excluded).
const SAFE_CHARSET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";

/// The pairing-code length, matching the Python `CODE_LENGTH`.
const CODE_LENGTH: usize = 6;

/// The number of random bytes behind a generated API key, matching the Python
/// `secrets.token_urlsafe(32)`.
const API_KEY_RANDOM_BYTES: usize = 32;

/// The read view of the pairing document the pairing-info route projects. Only
/// the cloud-pair fields the route surfaces are typed; every other key is
/// tolerated. Missing / unparseable file reads as the all-`None` unpaired
/// default, matching the Python `get_info()` fallback shape.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct PairingDoc {
    #[serde(default)]
    pub paired: bool,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub owner_id: Option<String>,
    #[serde(default)]
    pub paired_at: Option<f64>,
    #[serde(default)]
    pub pairing_code: Option<String>,
    #[serde(default)]
    pub pending_api_key: Option<String>,
}

impl PairingDoc {
    /// Read the current document from `path`. A missing or unparseable file reads
    /// as the unpaired default (never an error), so the guaranteed-200 contract
    /// holds.
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => PairingDoc::default(),
        }
    }

    /// Whether the agent is cloud-paired.
    pub fn is_paired(&self) -> bool {
        self.paired
    }

    /// The `owner_id` the pairing-info route reports — only when paired, matching
    /// the Python `get_info()`, which omits it (returns `None`) while unpaired.
    pub fn info_owner_id(&self) -> Option<String> {
        if self.paired {
            self.owner_id.clone()
        } else {
            None
        }
    }

    /// The `paired_at` the pairing-info route reports — only when paired.
    pub fn info_paired_at(&self) -> Option<f64> {
        if self.paired {
            self.paired_at
        } else {
            None
        }
    }

    /// The `pairing_code` the pairing-info route reports — only when UNPAIRED.
    /// The Python `get_info()` returns the live code while unpaired and omits it
    /// (`None`) while paired.
    pub fn info_pairing_code(&self) -> Option<String> {
        if self.paired {
            None
        } else {
            self.pairing_code.clone()
        }
    }
}

/// Generate a human-friendly pairing code, matching `PairingManager.generate_code`
/// (`secrets.choice(SAFE_CHARSET)` × `CODE_LENGTH`). Uniform over the charset via
/// rejection sampling so no character is biased by the modulo.
///
/// Fails closed: a `getrandom` error propagates rather than degrading to a
/// predictable code. The code is credential-adjacent (it gates the cloud claim),
/// so a guessable value is worse than no value — the caller surfaces the error.
pub fn generate_code() -> Result<String, getrandom::Error> {
    let mut out = String::with_capacity(CODE_LENGTH);
    let n = SAFE_CHARSET.len() as u8;
    // The largest multiple of `n` that fits in a byte; bytes at/above it are
    // rejected so the kept range maps uniformly onto the charset.
    let limit = (256u16 - (256u16 % n as u16)) as u8;
    while out.len() < CODE_LENGTH {
        let mut b = [0u8; 1];
        getrandom::getrandom(&mut b)?;
        if b[0] < limit {
            out.push(SAFE_CHARSET[(b[0] % n) as usize] as char);
        }
    }
    Ok(out)
}

/// Generate a secure API key, matching `PairingManager.generate_api_key`:
/// `"ados_"` + url-safe base64 (no padding) of 32 random bytes. The encoding,
/// length, and prefix match `secrets.token_urlsafe(32)` byte-for-byte in format
/// (the random bytes differ, as they do between any two Python calls).
///
/// Fails closed: a `getrandom` error propagates rather than emitting a key over a
/// fixed/all-zero buffer. A predictable api_key is a credential leak (the LAN
/// claim returns it to the operator and the agent trusts it forever), so the
/// caller surfaces the error rather than shipping a guessable key.
pub fn generate_api_key() -> Result<String, getrandom::Error> {
    use base64::Engine as _;
    let mut bytes = [0u8; API_KEY_RANDOM_BYTES];
    getrandom::getrandom(&mut bytes)?;
    let body = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    Ok(format!("ados_{body}"))
}

/// The four-key object the claim writer persists, in the Python insertion order
/// (`paired`, `api_key`, `owner_id`, `paired_at`). A struct (not a `serde_json`
/// map) so the on-disk key order is the Python order regardless of the
/// `serde_json` `preserve_order` feature.
#[derive(Serialize)]
struct ClaimedState<'a> {
    paired: bool,
    api_key: &'a str,
    owner_id: &'a str,
    paired_at: f64,
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
}

impl std::fmt::Display for ClaimError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClaimError::KeyGen(e) => write!(f, "key generation failed: {e}"),
            ClaimError::Persist(e) => write!(f, "{e}"),
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
/// claim never emits a predictable/all-zero key.
///
/// `now` is the `paired_at` timestamp (unix seconds, fractional), passed in so a
/// test can pin it; production passes the wall clock.
pub fn claim(path: &Path, user_id: &str, now: f64) -> Result<ClaimOutcome, ClaimError> {
    let current = PairingDoc::load(path);
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
    let state = ClaimedState {
        paired: true,
        api_key: &api_key,
        owner_id: user_id,
        paired_at: now,
    };
    let body = serde_json::to_vec_pretty(&state)
        .map_err(|e| ClaimError::Persist(std::io::Error::other(e.to_string())))?;
    atomic_write_0600(path, &body).map_err(ClaimError::Persist)?;
    Ok(ClaimOutcome { api_key })
}

/// Clear pairing state, mirroring `PairingManager.unpair`: write an empty object
/// `{}`. The Python writer resets the in-memory state to `{}` then saves, so the
/// on-disk result is the two-byte `{}` (pretty-printed by `json.dumps(..,
/// indent=2)`, which still emits `{}` for an empty dict).
pub fn unpair(path: &Path) -> std::io::Result<()> {
    atomic_write_0600(path, b"{}")
}

/// Persist a fresh pairing code, mirroring the `code`-writing branch of
/// `PairingManager.get_or_create_code` after an unpair: write
/// `{pairing_code, code_created_at}` (the Python `unpair` then `get_or_create_code`
/// sequence). Returns the new code. `now` is the `code_created_at` timestamp.
///
/// Fails closed on a `getrandom` error: a code is credential-adjacent, so the
/// caller surfaces the error rather than persisting a predictable code.
pub fn write_new_code(path: &Path, now: f64) -> std::io::Result<String> {
    #[derive(Serialize)]
    struct CodeState<'a> {
        pairing_code: &'a str,
        code_created_at: f64,
    }
    let code = generate_code().map_err(|e| std::io::Error::other(e.to_string()))?;
    let state = CodeState {
        pairing_code: &code,
        code_created_at: now,
    };
    let body =
        serde_json::to_vec_pretty(&state).map_err(|e| std::io::Error::other(e.to_string()))?;
    atomic_write_0600(path, &body)?;
    Ok(code)
}

/// Atomic 0600 write: temp sibling + write + flush + fsync + rename, mirroring
/// the Python `atomic_write_bytes(mode=0o600)`. The mode is set on the temp file
/// before the rename so the final file is owner-only the instant it appears.
fn atomic_write_0600(path: &Path, body: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("ados-pairing");
    let tmp = parent.join(format!("{}.{}.tmp", file_name, std::process::id()));

    let write_result = (|| -> std::io::Result<()> {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(body)?;
        f.flush()?;
        f.sync_all()?;
        Ok(())
    })();

    if write_result.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return write_result;
    }
    // Belt-and-braces: ensure the mode is 0600 even on a platform/umask where the
    // open mode did not stick. No-op when it already is.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn read_json(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn generate_code_is_six_safe_chars() {
        for _ in 0..200 {
            let code = generate_code().expect("getrandom must succeed on the test host");
            assert_eq!(code.len(), CODE_LENGTH, "code {code} wrong length");
            for c in code.bytes() {
                assert!(
                    SAFE_CHARSET.contains(&c),
                    "code {code} has out-of-charset byte {c}"
                );
            }
        }
    }

    #[test]
    fn generate_api_key_has_the_python_shape() {
        let key = generate_api_key().expect("getrandom must succeed on the test host");
        assert!(key.starts_with("ados_"), "key {key} missing prefix");
        // ados_ (5) + 32 bytes url-safe-no-pad base64 (ceil(32*4/3)=43) = 48.
        assert_eq!(key.len(), 48, "key {key} wrong length");
        let body = &key["ados_".len()..];
        // url-safe alphabet, no padding.
        assert!(
            body.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "key body {body} has a non-url-safe char"
        );
        assert!(!body.contains('='), "url-safe-no-pad must have no padding");
    }

    #[test]
    fn generated_api_keys_are_random_and_well_formed() {
        // Two fresh keys must differ — a fixed/all-zero fallback would make them
        // equal. The key returns a Result (fail-closed on a getrandom error); the
        // success path always yields a prefixed key of the right length.
        let a = generate_api_key().expect("getrandom must succeed on the test host");
        let b = generate_api_key().expect("getrandom must succeed on the test host");
        assert_ne!(a, b, "two generated keys must differ (no fixed fallback)");
        for key in [&a, &b] {
            assert!(key.starts_with("ados_"), "key {key} missing prefix");
            assert_eq!(key.len(), 48, "key {key} wrong length");
        }
        // The all-zero-buffer key that the removed fallback would emit, proving no
        // code path can produce it: ados_ + base64(32 zero bytes).
        let all_zero = {
            use base64::Engine as _;
            let body = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode([0u8; API_KEY_RANDOM_BYTES]);
            format!("ados_{body}")
        };
        assert_ne!(a, all_zero, "a generated key must not be the all-zero key");
        assert_ne!(b, all_zero, "a generated key must not be the all-zero key");
    }

    #[test]
    fn claim_prefers_the_pending_key_and_writes_the_four_python_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairing.json");
        std::fs::write(
            &path,
            r#"{"pairing_code":"ABC234","code_created_at":123.0,"pending_api_key":"ados_PENDING"}"#,
        )
        .unwrap();

        let out = claim(&path, "user-99", 1700000000.5).unwrap();
        // The pending key is preferred verbatim.
        assert_eq!(out.api_key, "ados_PENDING");

        let on_disk = read_json(&path);
        // Exactly the four Python keys, the code + pending key dropped.
        let keys: std::collections::BTreeSet<_> =
            on_disk.as_object().unwrap().keys().cloned().collect();
        let want: std::collections::BTreeSet<_> = ["paired", "api_key", "owner_id", "paired_at"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            keys, want,
            "claimed pairing.json keys differ from PairingManager"
        );
        assert_eq!(on_disk["paired"], Value::Bool(true));
        assert_eq!(on_disk["api_key"], Value::String("ados_PENDING".into()));
        assert_eq!(on_disk["owner_id"], Value::String("user-99".into()));
        assert_eq!(on_disk["paired_at"], serde_json::json!(1700000000.5));
        assert!(on_disk.get("pairing_code").is_none());
        assert!(on_disk.get("pending_api_key").is_none());
    }

    #[test]
    fn reclaim_of_a_paired_agent_returns_the_existing_key_without_rotating() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairing.json");
        // Already paired with a live key, and a DIFFERENT pending key present
        // (e.g. the pair code rotated since). A re-claim must return the
        // existing key, never the pending one, and never mint a new one — so a
        // client already holding the key is not silently locked out.
        std::fs::write(
            &path,
            r#"{"paired":true,"api_key":"ados_LIVE","owner_id":"op","paired_at":1.0,"pending_api_key":"ados_OTHER"}"#,
        )
        .unwrap();
        let out = claim(&path, "someone-else", 2.0).unwrap();
        assert_eq!(
            out.api_key, "ados_LIVE",
            "re-claim must reveal the live key"
        );
        let on_disk = read_json(&path);
        assert_eq!(on_disk["api_key"], Value::String("ados_LIVE".into()));
        assert_eq!(
            on_disk["owner_id"],
            Value::String("op".into()),
            "re-claim must not reassign ownership"
        );
    }

    #[test]
    fn claim_mints_a_fresh_key_when_no_pending_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairing.json");
        std::fs::write(&path, r#"{"pairing_code":"ABC234","code_created_at":1.0}"#).unwrap();
        let out = claim(&path, "user-1", 42.0).unwrap();
        assert!(out.api_key.starts_with("ados_"));
        assert_eq!(out.api_key.len(), 48);
        let on_disk = read_json(&path);
        assert_eq!(on_disk["api_key"], Value::String(out.api_key.clone()));
    }

    #[test]
    fn claim_writes_the_four_keys_in_the_python_insertion_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairing.json");
        claim(&path, "u", 1.0).unwrap();
        // The serialized struct order is paired, api_key, owner_id, paired_at —
        // the Python dict insertion order. Assert the raw text key order.
        let text = std::fs::read_to_string(&path).unwrap();
        let p = text.find("\"paired\"").unwrap();
        let k = text.find("\"api_key\"").unwrap();
        let o = text.find("\"owner_id\"").unwrap();
        let a = text.find("\"paired_at\"").unwrap();
        assert!(
            p < k && k < o && o < a,
            "key order drifted from PairingManager: {text}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn claim_persists_0600() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairing.json");
        claim(&path, "u", 1.0).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "pairing.json must be 0600");
    }

    #[test]
    fn unpair_writes_an_empty_object() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairing.json");
        std::fs::write(
            &path,
            r#"{"paired":true,"api_key":"k","owner_id":"o","paired_at":1.0}"#,
        )
        .unwrap();
        unpair(&path).unwrap();
        let on_disk = read_json(&path);
        assert_eq!(on_disk, serde_json::json!({}), "unpair must write {{}}");
    }

    #[cfg(unix)]
    #[test]
    fn unpair_persists_0600() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairing.json");
        unpair(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn write_new_code_persists_a_fresh_six_char_code() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairing.json");
        let code = write_new_code(&path, 99.0).unwrap();
        assert_eq!(code.len(), CODE_LENGTH);
        let on_disk = read_json(&path);
        assert_eq!(on_disk["pairing_code"], Value::String(code));
        assert_eq!(on_disk["code_created_at"], serde_json::json!(99.0));
    }

    #[test]
    fn load_reads_the_cloud_pair_fields_when_paired() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairing.json");
        std::fs::write(
            &path,
            r#"{"paired":true,"api_key":"k","owner_id":"user-42","paired_at":1700000000.0,"pairing_code":"ZZ"}"#,
        )
        .unwrap();
        let doc = PairingDoc::load(&path);
        assert!(doc.is_paired());
        assert_eq!(doc.info_owner_id(), Some("user-42".to_string()));
        assert_eq!(doc.info_paired_at(), Some(1700000000.0));
        // Paired → code omitted from info even if a stale code lingers in the file.
        assert_eq!(doc.info_pairing_code(), None);
    }

    #[test]
    fn load_reports_the_code_only_when_unpaired() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairing.json");
        std::fs::write(&path, r#"{"pairing_code":"ABC234","code_created_at":1.0}"#).unwrap();
        let doc = PairingDoc::load(&path);
        assert!(!doc.is_paired());
        assert_eq!(doc.info_pairing_code(), Some("ABC234".to_string()));
        // Unpaired → owner/paired_at omitted.
        assert_eq!(doc.info_owner_id(), None);
        assert_eq!(doc.info_paired_at(), None);
    }

    #[test]
    fn load_of_an_absent_file_is_the_unpaired_default() {
        let dir = tempfile::tempdir().unwrap();
        let doc = PairingDoc::load(&dir.path().join("absent.json"));
        assert!(!doc.is_paired());
        assert_eq!(doc.info_owner_id(), None);
        assert_eq!(doc.info_pairing_code(), None);
    }
}
