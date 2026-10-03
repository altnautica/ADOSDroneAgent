//! Persisted HMAC issuer secret and per-plugin token delivery.
//!
//! A subprocess plugin runs as its own systemd unit, a separate process from
//! the host daemon. For the daemon to verify a token a runner presents at the
//! `hello` handshake, both sides must mint and verify against the *same* HMAC
//! secret. A per-process random secret cannot do that across two processes, so
//! the secret is persisted once to a 0600 file under `/etc/ados/secrets` and
//! loaded by both the daemon and the unit-generation path. The signing payload
//! and verify are unchanged (`ados-protocol::plugin`); only the key source
//! moves from per-process-random to a shared on-disk key.
//!
//! The secret is created atomically (written to a private temp file, then
//! linked into place, which fails if another process got there first), so a
//! concurrent first use by two processes converges on one secret and no reader
//! ever sees a half-written file. An existing secret that does not decode is an
//! error, never silently replaced: replacing it would split the processes that
//! already loaded the old one from the ones that load the new one.
//!
//! Token delivery is a systemd credential. The host writes a root-owned 0600
//! file of `KEY=VALUE` lines (the token, the socket path, the paired device id
//! and the plugin's data dir), and the unit loads it with
//! `LoadCredential=ados-plugin-token:<path>`. systemd copies it into the
//! process's private `$CREDENTIALS_DIRECTORY`, readable by that plugin's own
//! user only, so the token never sits in the process environment where
//! `/proc/<pid>/environ` would expose it, and never appears in the unit file or
//! on a command line. The Python runner and the Rust SDK read
//! `$CREDENTIALS_DIRECTORY/ados-plugin-token`. The file is rewritten
//! atomically with a fresh token on each start and on every rotation; a live
//! session receives rotations over its socket.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ados_protocol::plugin::{CapabilityToken, TokenIssuer, TOKEN_TTL_SECONDS};

use crate::server::DEFAULT_SOCKET_DIR;

/// The persisted HMAC issuer secret, 0600 owner-only. Hex-encoded 32 bytes.
/// Both the daemon and the unit-generation path load this so a token minted
/// when a unit is (re)written verifies in the daemon that serves the socket.
pub const PLUGIN_TOKEN_SECRET_PATH: &str = "/etc/ados/secrets/plugin-token-secret";

/// The directory per-plugin token credentials are written to. On tmpfs so the
/// token (and its file) never survive a reboot and rotate on each start.
pub const PLUGIN_TOKEN_CREDENTIAL_DIR: &str = DEFAULT_SOCKET_DIR;

/// The systemd credential id a plugin unit loads its token file under; the
/// file appears as `$CREDENTIALS_DIRECTORY/<this>` inside the plugin process.
pub const TOKEN_CREDENTIAL_NAME: &str = "ados-plugin-token";

/// Length of the issuer secret in bytes (`secrets.token_bytes(32)`).
const SECRET_LEN: usize = 32;

/// The credential key carrying the plugin's socket path.
pub const ENV_SOCKET: &str = "ADOS_PLUGIN_SOCKET";
/// The credential key carrying the capability token.
pub const ENV_TOKEN: &str = "ADOS_PLUGIN_TOKEN";
/// The credential key carrying the paired device id. Empty on an unpaired
/// node; the runner then treats the plugin as node-scoped rather than per-drone.
pub const ENV_AGENT_ID: &str = "ADOS_PLUGIN_AGENT_ID";
/// The credential key carrying the plugin's per-drone data directory.
pub const ENV_DATA_DIR: &str = "ADOS_PLUGIN_DATA_DIR";
/// Default base of the persistent plugin data tree (`ADOS_PLUGIN_DATA_DIR_ROOT`
/// overrides it, see [`crate::supervisor::Paths`]). Mirrors the Python
/// `PLUGIN_DATA_DIR`. Each plugin's unit binds only its own `<base>/<id>`
/// writable.
pub const PLUGIN_DATA_DIR: &str = "/var/ados/plugin-data";

/// The plugin's data directory, matching the Python `_data_dir_for`: node-scoped
/// at `<data_root>/<id>`, or per-drone at `<data_root>/<id>/drones/<agent_id>`
/// when paired.
pub fn plugin_data_dir(data_root: &Path, plugin_id: &str, agent_id: &str) -> PathBuf {
    let base = data_root.join(plugin_id);
    if agent_id.is_empty() {
        base
    } else {
        base.join("drones").join(agent_id)
    }
}

/// The absolute path of the token credential a plugin's unit loads through
/// `LoadCredential=`. One file per plugin so a unit restart rewrites only that
/// plugin's token.
pub fn token_credential_path(plugin_id: &str, credential_dir: Option<&Path>) -> PathBuf {
    let dir = credential_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(PLUGIN_TOKEN_CREDENTIAL_DIR));
    dir.join(format!("{plugin_id}.token"))
}

/// Load the shared issuer secret, creating it on first use.
///
/// An existing file is read and hex-decoded; one that does not hold a valid
/// secret of the expected length is an error, never replaced. A missing (or
/// empty) file gets a fresh 32-byte secret written to a private temp file and
/// linked into place, so of two processes creating it at once exactly one
/// wins and the other reads the winner's secret. The directory is created
/// 0700 if absent.
pub fn load_or_create_secret(path: &Path) -> std::io::Result<Vec<u8>> {
    match std::fs::read_to_string(path) {
        Ok(text) if !text.trim().is_empty() => return decode_secret(path, &text),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut secret = vec![0u8; SECRET_LEN];
    getrandom::fill(&mut secret).map_err(|e| std::io::Error::other(e.to_string()))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        set_dir_mode(parent);
    }
    let tmp = temp_sibling(path);
    write_owner_only(&tmp, hex::encode(&secret).as_bytes())?;
    let placed = match std::fs::hard_link(&tmp, path) {
        Ok(()) => Ok(secret),
        // Another process created it first, or an empty file sat there. An
        // empty file is replaced; a populated one is the winner's secret.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = std::fs::read_to_string(path)?;
            if existing.trim().is_empty() {
                std::fs::rename(&tmp, path).map(|()| secret)
            } else {
                decode_secret(path, &existing)
            }
        }
        Err(e) => Err(e),
    };
    let _ = std::fs::remove_file(&tmp);
    placed
}

fn decode_secret(path: &Path, text: &str) -> std::io::Result<Vec<u8>> {
    match hex::decode(text.trim()) {
        Ok(bytes) if bytes.len() == SECRET_LEN => Ok(bytes),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "{} does not hold a {SECRET_LEN}-byte hex secret; refusing to replace it",
                path.display()
            ),
        )),
    }
}

/// Build the shared [`TokenIssuer`] from the persisted secret, creating the
/// secret on first use. This is the single constructor both the daemon and the
/// unit-generation path use so they share one HMAC key.
pub fn shared_issuer(secret_path: &Path) -> std::io::Result<TokenIssuer> {
    let secret = load_or_create_secret(secret_path)?;
    Ok(TokenIssuer::new(secret))
}

/// Mint a token for a plugin from the shared issuer and write the 0600 token
/// credential its unit loads. The file holds the `KEY=VALUE` lines the runner
/// reads (`ADOS_PLUGIN_TOKEN`, `ADOS_PLUGIN_SOCKET`, `ADOS_PLUGIN_AGENT_ID`,
/// `ADOS_PLUGIN_DATA_DIR`). Returns the minted token so a caller (or a test)
/// can assert it verifies against the same issuer.
///
/// `socket_path` is the per-plugin socket the daemon serves; `granted_caps` are
/// the permissions the install record grants; `data_root` and `agent_id` place
/// the plugin's data dir. The token rotates each call (fresh session id +
/// issued_at), matching the "rotate on every plugin restart and on every
/// permission change" contract.
pub fn write_token_credential(
    issuer: &TokenIssuer,
    plugin_id: &str,
    granted_caps: &BTreeSet<String>,
    socket_path: &Path,
    data_root: &Path,
    agent_id: &str,
    credential_dir: Option<&Path>,
) -> std::io::Result<CapabilityToken> {
    let token = issuer.mint(plugin_id, granted_caps, TOKEN_TTL_SECONDS);
    let path = token_credential_path(plugin_id, credential_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // The agent id and data dir ride here rather than on the unit's ExecStart:
    // the unit is rendered once, but the paired device id is a runtime fact, and
    // this file is already rewritten per start.
    let data_dir = plugin_data_dir(data_root, plugin_id, agent_id);
    let body = format!(
        "{ENV_TOKEN}={token}\n\
         {ENV_SOCKET}={socket}\n\
         {ENV_AGENT_ID}={agent_id}\n\
         {ENV_DATA_DIR}={data_dir}\n",
        token = token.to_token_string(),
        socket = socket_path.display(),
        data_dir = data_dir.display(),
    );
    // Written beside the target and renamed over it, so systemd never loads a
    // half-written credential at a unit start that races a rotation.
    let tmp = temp_sibling(&path);
    write_owner_only(&tmp, body.as_bytes())?;
    std::fs::rename(&tmp, &path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    Ok(token)
}

/// Mints a plugin's current capability token from authoritative on-disk state.
///
/// The token is a snapshot of `(plugin_id, granted_caps, session, exp)` with a
/// [`TOKEN_TTL_SECONDS`] lifetime, so two things force a re-mint and this type
/// is what both go through:
///
/// * **Expiry.** The TTL is ten minutes and a plugin runs for the whole
///   flight. Nothing rotated the token, so every gated call from every plugin
///   started failing `token_expired` ten minutes after the host began serving
///   — the plugin process stayed up, `Restart=on-failure` never fired, and a
///   long-running geofence or follow-me plugin went quietly dead in the air.
/// * **A permission change.** A grant or revoke rewrites state; the live
///   token still carries the old set until it is re-minted.
///
/// Reading the grant set off state on every mint is what makes a revoke real:
/// the fresh token cannot carry a capability the operator just removed, and
/// the host re-gates the next request against it.
pub struct TokenMint {
    issuer: Arc<TokenIssuer>,
    state_path: PathBuf,
    socket_dir: PathBuf,
    data_root: PathBuf,
    device_id: String,
}

impl TokenMint {
    pub fn new(
        issuer: Arc<TokenIssuer>,
        state_path: PathBuf,
        socket_dir: PathBuf,
        data_root: PathBuf,
        device_id: String,
    ) -> Self {
        TokenMint {
            issuer,
            state_path,
            socket_dir,
            data_root,
            device_id,
        }
    }

    /// The plugin data root this mint places data dirs under.
    pub fn data_root(&self) -> &Path {
        &self.data_root
    }

    /// The plugin's socket path under this mint's socket dir.
    pub fn socket_path(&self, plugin_id: &str) -> PathBuf {
        crate::server::plugin_socket_path(&self.socket_dir, plugin_id)
    }

    /// Mint a fresh token for `plugin_id` from the current grant set and
    /// rewrite its 0600 token credential.
    ///
    /// Returns `None` when the plugin is not installed or is not in a state
    /// that should hold a token (disabled, failed, removed) — an expired token
    /// then stays expired, which is the correct answer for a plugin the
    /// operator has turned off. The credential is rewritten as well as the
    /// token returned, so a plugin that restarts after a rotation loads the
    /// live token rather than the one minted at the last daemon start.
    pub fn mint_current(&self, plugin_id: &str) -> Option<CapabilityToken> {
        let installs = crate::state::load_state(Some(&self.state_path));
        let install = crate::state::find_install(&installs, plugin_id)?;
        if !matches!(
            install.status,
            crate::state::PluginStatus::Enabled | crate::state::PluginStatus::Running
        ) {
            return None;
        }
        let caps = crate::state::granted_caps(install);
        let socket_path = self.socket_path(plugin_id);
        match write_token_credential(
            &self.issuer,
            plugin_id,
            &caps,
            &socket_path,
            &self.data_root,
            &self.device_id,
            Some(&self.socket_dir),
        ) {
            Ok(token) => Some(token),
            Err(e) => {
                tracing::warn!(
                    plugin_id,
                    error = %e,
                    "failed to write rotated plugin token credential"
                );
                None
            }
        }
    }

    /// Remove a plugin's token credential. Called when a plugin leaves the
    /// enabled/running states so a stale token does not sit on tmpfs.
    pub fn forget(&self, plugin_id: &str) {
        let _ = std::fs::remove_file(token_credential_path(plugin_id, Some(&self.socket_dir)));
    }
}

/// A private temp path beside `path`, unique to this process and call.
fn temp_sibling(path: &Path) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!(".{name}.{}.{n}.tmp", std::process::id()))
}

/// Create `path` owner-only (0600) and write `contents`. The file must not
/// exist: every caller writes a fresh temp file and moves it into place, so a
/// reader never sees a truncated or looser-mode file.
#[cfg(unix)]
fn write_owner_only(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(contents)?;
    f.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn write_owner_only(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, contents)
}

/// Best-effort 0700 on the secret directory. Linux-only; ignored elsewhere and
/// on any error (the file mode is the load-bearing protection).
fn set_dir_mode(dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(dir) {
            let mut perms = meta.permissions();
            perms.set_mode(0o700);
            let _ = std::fs::set_permissions(dir, perms);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn load_or_create_persists_a_stable_secret() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets/plugin-token-secret");
        let first = load_or_create_secret(&path).unwrap();
        assert_eq!(first.len(), SECRET_LEN);
        assert!(path.exists());
        // A second load reads the same persisted bytes (does not regenerate).
        let second = load_or_create_secret(&path).unwrap();
        assert_eq!(first, second);
    }

    #[cfg(unix)]
    #[test]
    fn secret_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugin-token-secret");
        load_or_create_secret(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "secret must be 0600");
    }

    #[test]
    fn a_malformed_existing_secret_is_refused_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugin-token-secret");
        std::fs::write(&path, b"not-hex-and-too-short").unwrap();
        assert!(load_or_create_secret(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"not-hex-and-too-short");
    }

    #[test]
    fn concurrent_first_use_converges_on_one_secret() {
        let dir = tempfile::tempdir().unwrap();
        let path = Arc::new(dir.path().join("plugin-token-secret"));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || load_or_create_secret(&path).unwrap())
            })
            .collect();
        let secrets: Vec<Vec<u8>> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert!(secrets.windows(2).all(|w| w[0] == w[1]));
        assert_eq!(load_or_create_secret(&path).unwrap(), secrets[0]);
    }

    #[test]
    fn the_token_credential_carries_the_runner_keys() {
        let dir = tempfile::tempdir().unwrap();
        let secret_path = dir.path().join("plugin-token-secret");
        let issuer = shared_issuer(&secret_path).unwrap();
        let sock = dir.path().join("plugins/com.example.demo.sock");
        let cred_dir = dir.path().join("plugins");
        let token = write_token_credential(
            &issuer,
            "com.example.demo",
            &caps(&["mavlink.read"]),
            &sock,
            Path::new("/var/ados/plugin-data"),
            "drone-abc",
            Some(&cred_dir),
        )
        .unwrap();

        let path = token_credential_path("com.example.demo", Some(&cred_dir));
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains(&format!("{ENV_TOKEN}={}", token.to_token_string())));
        assert!(body.contains(&format!("{ENV_SOCKET}={}", sock.display())));
        assert!(body.contains(&format!("{ENV_AGENT_ID}=drone-abc")));
        assert!(body.contains(&format!(
            "{ENV_DATA_DIR}=/var/ados/plugin-data/com.example.demo/drones/drone-abc"
        )));
    }

    #[cfg(unix)]
    #[test]
    fn a_rewritten_credential_is_owner_only_even_over_a_looser_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let secret_path = dir.path().join("plugin-token-secret");
        let issuer = shared_issuer(&secret_path).unwrap();
        let cred_dir = dir.path().join("plugins");
        std::fs::create_dir_all(&cred_dir).unwrap();
        let path = token_credential_path("com.example.x", Some(&cred_dir));
        std::fs::write(&path, b"ADOS_PLUGIN_TOKEN=stale\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        write_token_credential(
            &issuer,
            "com.example.x",
            &BTreeSet::new(),
            &dir.path().join("x.sock"),
            dir.path(),
            "",
            Some(&cred_dir),
        )
        .unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!std::fs::read_to_string(&path)
            .unwrap()
            .contains("ADOS_PLUGIN_TOKEN=stale"));
    }

    #[test]
    fn cross_process_mint_then_verify_with_persisted_secret() {
        // An issuer built from the persisted secret in "process A" (the
        // unit-generation path) mints a token; a fresh issuer built from the
        // SAME persisted secret in "process B" (the serving daemon) verifies it.
        let dir = tempfile::tempdir().unwrap();
        let secret_path = dir.path().join("plugin-token-secret");

        let minting_issuer = shared_issuer(&secret_path).unwrap();
        let token = minting_issuer.mint(
            "com.example.demo",
            &caps(&["mavlink.read"]),
            TOKEN_TTL_SECONDS,
        );

        let verifying_issuer = shared_issuer(&secret_path).unwrap();
        let now = token.issued_at + 1;
        assert!(
            verifying_issuer.verify(&token, now).is_ok(),
            "token minted from the persisted secret must verify in a separate issuer"
        );
    }
}
