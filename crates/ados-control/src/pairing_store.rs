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
//!
//! ## One writer at a time
//!
//! Every writer — the claim, unpair and code routes here and the Python
//! `PairingManager` — holds [`lock_writers`] across its read-check-write: a
//! process-wide async mutex plus an exclusive `flock` on the sibling
//! `pairing.json.lock` the Python writer takes too. Without it two concurrent
//! claims each minted and returned a different key, and a Python code
//! regeneration racing a claim could overwrite the paired document.

use std::io::Write as _;
use std::path::Path;

use serde::Serialize;

mod claim;

pub use claim::claim;
pub use claim::claim_with_key;
pub use claim::ClaimError;
pub use claim::ClaimOutcome;

/// Canonical pairing-state path. Mirrors `PAIRING_JSON`. The crate's auth reader
/// resolves the same file (its own `DEFAULT_PAIRING_PATH`); the daemon passes the
/// resolved path to both so they agree.
pub const PAIRING_JSON: &str = "/etc/ados/pairing.json";

/// The human-friendly pairing-code charset, matching the Python `SAFE_CHARSET`:
/// no ambiguous characters (`0/O/1/I/L` excluded).
const SAFE_CHARSET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";

/// The pairing-code length, matching the Python `CODE_LENGTH`.
pub const CODE_LENGTH: usize = 6;

/// The number of random bytes behind a generated API key, matching the Python
/// `secrets.token_urlsafe(32)`.
const API_KEY_RANDOM_BYTES: usize = 32;

/// How long an unpaired node's pairing code lives before it is regenerated.
/// Matches the Python `CODE_TTL`, so every surface shows the same code.
pub const CODE_TTL_SECONDS: f64 = 24.0 * 60.0 * 60.0;

/// How long a writer waits for another writer's lock. A holder keeps it for one
/// read plus one atomic write; the same bound the Python writer uses.
const WRITE_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Serialises this process's writers before they contend on the file lock, so
/// two requests in one process queue rather than poll.
static PROCESS_WRITERS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The lock file guarding `pairing_json`: the sibling `<name>.lock`.
pub fn lock_path_for(pairing_json: &Path) -> std::path::PathBuf {
    let mut name = pairing_json
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".lock");
    pairing_json.with_file_name(name)
}

/// Held while a writer reads, checks and rewrites `pairing.json`. Dropping it
/// releases both locks.
pub struct WriterLock {
    _process: tokio::sync::MutexGuard<'static, ()>,
    _file: std::fs::File,
}

/// Take the pairing write lock for `pairing_json`.
pub async fn lock_writers(pairing_json: &Path) -> std::io::Result<WriterLock> {
    let process = PROCESS_WRITERS.lock().await;
    let lock_path = lock_path_for(pairing_json);
    let file = tokio::task::spawn_blocking(move || acquire_file_lock(&lock_path))
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))??;
    Ok(WriterLock {
        _process: process,
        _file: file,
    })
}

/// The exclusive `flock`, polled until [`WRITE_LOCK_TIMEOUT`] (a blocking
/// `flock` cannot be given a deadline).
fn acquire_file_lock(lock_path: &Path) -> std::io::Result<std::fs::File> {
    if let Some(parent) = lock_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let file = opts.open(lock_path)?;
    let deadline = std::time::Instant::now() + WRITE_LOCK_TIMEOUT;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) => {
                if std::time::Instant::now() >= deadline {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!("{} held by another writer", lock_path.display()),
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e),
        }
    }
}

/// The read view of the pairing document the pairing-info route projects. Only
/// the cloud-pair fields the route surfaces are typed; every other key is
/// tolerated. An absent file reads as the all-`None` unpaired default; an
/// unreadable or malformed one is an error ([`PairingDoc::read`]), never the
/// unpaired default, because "unpaired" is what lets anyone on the LAN claim a
/// fresh key.
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
    pub code_created_at: Option<f64>,
    #[serde(default)]
    pub pending_api_key: Option<String>,
}

impl PairingDoc {
    /// Read the current document from `path`. An absent file is the unpaired
    /// default; any other read failure, a parse failure, or `paired:true` with
    /// no key is an error carrying the reason.
    pub fn read(path: &Path) -> Result<Self, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let value: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        if !value.is_object() {
            return Err(format!("{}: not a JSON object", path.display()));
        }
        let doc: Self =
            serde_json::from_value(value).map_err(|e| format!("{}: {e}", path.display()))?;
        if doc.paired && doc.api_key.as_deref().unwrap_or("").is_empty() {
            return Err(format!("{}: paired with no key", path.display()));
        }
        Ok(doc)
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

    /// The code an unpaired node holds, when it is still inside
    /// [`CODE_TTL_SECONDS`] at `now`. `None` when paired, absent or expired; a
    /// caller that needs one regenerates it under [`lock_writers`].
    pub fn live_code(&self, now: f64) -> Option<String> {
        if self.paired {
            return None;
        }
        let code = self.pairing_code.clone().filter(|c| !c.is_empty())?;
        let created = self.code_created_at.unwrap_or(0.0);
        (now - created < CODE_TTL_SECONDS).then_some(code)
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
        getrandom::fill(&mut b)?;
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
    getrandom::fill(&mut bytes)?;
    let body = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    Ok(format!("ados_{body}"))
}

/// Clear pairing state, mirroring `PairingManager.unpair`: write an empty object
/// `{}`. The Python writer resets the in-memory state to `{}` then saves, so the
/// on-disk result is the two-byte `{}` (pretty-printed by `json.dumps(..,
/// indent=2)`, which still emits `{}` for an empty dict).
///
/// The relay peer secret belongs to the pairing it was offered under, so it is
/// removed too: the next ground station's offer must be accepted, and the
/// previous holder's relay tickets must stop verifying.
pub fn unpair(path: &Path, relay_secret: &Path) -> std::io::Result<()> {
    atomic_write_0600(path, b"{}")?;
    remove_relay_secret(relay_secret)
}

/// Remove the relay peer secret. Absent is success.
pub fn remove_relay_secret(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Make sure an unpaired node holds a live pairing code AND a pending API key,
/// mirroring `PairingManager.get_or_create_code`: the code and the key always
/// travel together, so the key a cloud pair beacon advertises is the key a
/// claim later persists. Returns the code on file afterwards. `now` is the
/// `code_created_at` stamp of a newly minted code. The caller holds
/// [`lock_writers`].
///
/// A code still inside [`CODE_TTL_SECONDS`] is kept; an absent or expired one
/// is replaced. A pending key on file is kept; an absent one is minted. When
/// both are already present nothing is written.
///
/// Refuses a paired or unreadable document: a code belongs to an unpaired
/// node, and overwriting a file that could not be read may destroy a live
/// pairing. Fails closed on a `getrandom` error: a code and a key are
/// credential-adjacent, so the caller surfaces the error rather than
/// persisting a predictable value.
pub fn ensure_code(path: &Path, now: f64) -> std::io::Result<String> {
    let current = PairingDoc::read(path).map_err(std::io::Error::other)?;
    if current.paired {
        return Err(std::io::Error::other("already paired"));
    }
    let pending = current.pending_api_key.clone().filter(|k| !k.is_empty());
    match (current.live_code(now), pending) {
        (Some(code), Some(_)) => Ok(code),
        (Some(code), None) => {
            let created = current.code_created_at.unwrap_or(now);
            write_code_state(path, &code, created, &mint_key()?)?;
            Ok(code)
        }
        (None, pending) => {
            let code = generate_code().map_err(|e| std::io::Error::other(e.to_string()))?;
            let key = match pending {
                Some(key) => key,
                None => mint_key()?,
            };
            write_code_state(path, &code, now, &key)?;
            Ok(code)
        }
    }
}

/// Replace the pairing code after an unpair, with a pending key beside it.
/// The caller holds [`lock_writers`] and has just cleared the document.
pub fn write_new_code(path: &Path, now: f64) -> std::io::Result<String> {
    let current = PairingDoc::read(path).map_err(std::io::Error::other)?;
    if current.paired {
        return Err(std::io::Error::other("already paired"));
    }
    let code = generate_code().map_err(|e| std::io::Error::other(e.to_string()))?;
    let key = match current.pending_api_key.filter(|k| !k.is_empty()) {
        Some(key) => key,
        None => mint_key()?,
    };
    write_code_state(path, &code, now, &key)?;
    Ok(code)
}

fn mint_key() -> std::io::Result<String> {
    generate_api_key().map_err(|e| std::io::Error::other(e.to_string()))
}

/// Write the unpaired document: the code, its creation stamp and the pending
/// key, in the order the Python writer leaves them.
fn write_code_state(
    path: &Path,
    code: &str,
    created_at: f64,
    pending_key: &str,
) -> std::io::Result<()> {
    #[derive(Serialize)]
    struct CodeState<'a> {
        pairing_code: &'a str,
        code_created_at: f64,
        pending_api_key: &'a str,
    }
    let state = CodeState {
        pairing_code: code,
        code_created_at: created_at,
        pending_api_key: pending_key,
    };
    let body =
        serde_json::to_vec_pretty(&state).map_err(|e| std::io::Error::other(e.to_string()))?;
    atomic_write_0600(path, &body)
}

/// Atomic 0600 write: temp sibling + write + flush + fsync + rename, mirroring
/// the Python `atomic_write_bytes(mode=0o600)`. The mode is set on the temp file
/// before the rename so the final file is owner-only the instant it appears.
/// Shared with [`crate::dashboard_pin`], which persists its record with the same
/// atomic-0600 hygiene.
pub(crate) fn atomic_write_0600(path: &Path, body: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("ados-pairing");
    // A per-call unique suffix (pid + a monotonic counter) so two concurrent
    // same-process writers to the same target never collide on one temp file
    // (which would corrupt one write, and — for the MCP token store — could lose
    // a revocation racing a mint).
    static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = parent.join(format!("{}.{}.{}.tmp", file_name, std::process::id(), seq));

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
    // The rename is only durable once the directory entry is: without this a
    // power cut right after a claim or unpair answered can bring the old record
    // back on the next boot.
    std::fs::File::open(parent)?.sync_all()
}

#[cfg(test)]
mod tests;
