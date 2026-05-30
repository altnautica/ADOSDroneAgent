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
//! Token delivery to the runner uses a 0600 environment file the systemd unit
//! references via `EnvironmentFile=`. The runner already reads
//! `ADOS_PLUGIN_TOKEN` / `ADOS_PLUGIN_SOCKET` from its environment (the Python
//! `runner.py` click options default to `os.environ.get(...)`, and the Rust SDK
//! `RunnerArgs::parse` falls back to the same env vars). The token never
//! appears in the world-readable unit file or in `/proc/<pid>/cmdline`: the
//! socket path is a static `Environment=` line in the unit, and the short-lived
//! token rides in the owner-only env file that is rewritten on each start.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use ados_protocol::plugin::{CapabilityToken, TokenIssuer, TOKEN_TTL_SECONDS};

use crate::server::DEFAULT_SOCKET_DIR;

/// The persisted HMAC issuer secret, 0600 owner-only. Hex-encoded 32 bytes.
/// Both the daemon and the unit-generation path load this so a token minted
/// when a unit is (re)written verifies in the daemon that serves the socket.
pub const PLUGIN_TOKEN_SECRET_PATH: &str = "/etc/ados/secrets/plugin-token-secret";

/// The environment-file directory for per-plugin token delivery. On tmpfs so
/// the token (and its file) never survive a reboot and rotate on each start.
pub const PLUGIN_TOKEN_ENV_DIR: &str = DEFAULT_SOCKET_DIR;

/// Length of the issuer secret in bytes (`secrets.token_bytes(32)`).
const SECRET_LEN: usize = 32;

/// The env-var the runner reads for its socket path.
pub const ENV_SOCKET: &str = "ADOS_PLUGIN_SOCKET";
/// The env-var the runner reads for its capability token.
pub const ENV_TOKEN: &str = "ADOS_PLUGIN_TOKEN";

/// The absolute env-file path a plugin's unit references via `EnvironmentFile=`.
/// One file per plugin so a unit restart rewrites only that plugin's token.
pub fn token_env_path(plugin_id: &str, env_dir: Option<&Path>) -> PathBuf {
    let dir = env_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(PLUGIN_TOKEN_ENV_DIR));
    dir.join(format!("{plugin_id}.token.env"))
}

/// Load the shared issuer secret, creating it on first use.
///
/// If the file exists it is read and hex-decoded. If it is missing (or the
/// content is not a valid hex secret of the expected length) a fresh 32-byte
/// secret is generated, written 0600, and returned. The directory is created
/// with 0700 if absent. Mirrors the Python `secrets.token_bytes(32)` default
/// but persists it so cross-process mint/verify works.
pub fn load_or_create_secret(path: &Path) -> std::io::Result<Vec<u8>> {
    if let Ok(text) = std::fs::read_to_string(path) {
        let trimmed = text.trim();
        if let Ok(bytes) = hex::decode(trimmed) {
            if bytes.len() == SECRET_LEN {
                return Ok(bytes);
            }
        }
        // A short / malformed secret is treated as absent and regenerated; a
        // stale or truncated file must not wedge the host on a verify mismatch.
    }
    let mut secret = vec![0u8; SECRET_LEN];
    getrandom::getrandom(&mut secret).map_err(|e| std::io::Error::other(e.to_string()))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        set_dir_mode(parent);
    }
    write_owner_only(path, hex::encode(&secret).as_bytes())?;
    Ok(secret)
}

/// Build the shared [`TokenIssuer`] from the persisted secret, creating the
/// secret on first use. This is the single constructor both the daemon and the
/// unit-generation path use so they share one HMAC key.
pub fn shared_issuer(secret_path: &Path) -> std::io::Result<TokenIssuer> {
    let secret = load_or_create_secret(secret_path)?;
    Ok(TokenIssuer::new(secret))
}

/// Mint a token for a plugin from the shared issuer and write the 0600 env file
/// the unit references. The env file holds the two `KEY=VALUE` lines the runner
/// reads (`ADOS_PLUGIN_TOKEN`, `ADOS_PLUGIN_SOCKET`). Returns the minted token
/// so a caller (or a test) can assert it verifies against the same issuer.
///
/// `socket_path` is the per-plugin socket the daemon serves; `granted_caps` are
/// the permissions the install record grants. The token rotates each call
/// (fresh session id + issued_at), matching the "rotate on every plugin restart
/// and on every permission change" contract.
pub fn write_token_env(
    issuer: &TokenIssuer,
    plugin_id: &str,
    granted_caps: &BTreeSet<String>,
    socket_path: &Path,
    env_dir: Option<&Path>,
) -> std::io::Result<CapabilityToken> {
    let token = issuer.mint(plugin_id, granted_caps, TOKEN_TTL_SECONDS);
    let path = token_env_path(plugin_id, env_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = format!(
        "{ENV_TOKEN}={token}\n{ENV_SOCKET}={socket}\n",
        token = token.to_token_string(),
        socket = socket_path.display(),
    );
    write_owner_only(&path, body.as_bytes())?;
    Ok(token)
}

/// Write a file with owner-only (0600) permissions. On unix the mode is set at
/// open time, before any group/other could read the secret; off unix the file
/// is written without a mode set so the crate still builds and tests on a dev
/// host that is not unix.
#[cfg(unix)]
fn write_owner_only(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(contents)?;
    f.flush()
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
    fn malformed_secret_is_regenerated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugin-token-secret");
        std::fs::write(&path, b"not-hex-and-too-short").unwrap();
        let secret = load_or_create_secret(&path).unwrap();
        assert_eq!(secret.len(), SECRET_LEN);
        // The file is now a valid hex secret of the right length.
        let reread = load_or_create_secret(&path).unwrap();
        assert_eq!(secret, reread);
    }

    #[test]
    fn write_token_env_emits_runner_env_keys() {
        let dir = tempfile::tempdir().unwrap();
        let secret_path = dir.path().join("plugin-token-secret");
        let issuer = shared_issuer(&secret_path).unwrap();
        let sock = dir.path().join("plugins/com.example.demo.sock");
        let env_dir = dir.path().join("plugins");
        let token = write_token_env(
            &issuer,
            "com.example.demo",
            &caps(&["mavlink.read"]),
            &sock,
            Some(&env_dir),
        )
        .unwrap();

        let env_path = token_env_path("com.example.demo", Some(&env_dir));
        let body = std::fs::read_to_string(&env_path).unwrap();
        // The env file carries the two keys the runner reads.
        assert!(body.contains(&format!("{ENV_TOKEN}={}", token.to_token_string())));
        assert!(body.contains(&format!("{ENV_SOCKET}={}", sock.display())));
    }

    #[cfg(unix)]
    #[test]
    fn token_env_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let secret_path = dir.path().join("plugin-token-secret");
        let issuer = shared_issuer(&secret_path).unwrap();
        let sock = dir.path().join("x.sock");
        let env_dir = dir.path().join("plugins");
        write_token_env(&issuer, "com.example.x", &BTreeSet::new(), &sock, Some(&env_dir)).unwrap();
        let env_path = token_env_path("com.example.x", Some(&env_dir));
        let mode = std::fs::metadata(&env_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "token env file must be 0600");
    }

    #[test]
    fn cross_process_mint_then_verify_with_persisted_secret() {
        // The core of the cross-process fix: an issuer built from the persisted
        // secret in "process A" (the unit-generation path) mints a token; a
        // fresh issuer built from the SAME persisted secret in "process B" (the
        // serving daemon) verifies it. This is exactly the daemon-vs-runner-unit
        // split the per-process random secret could not satisfy.
        let dir = tempfile::tempdir().unwrap();
        let secret_path = dir.path().join("plugin-token-secret");

        let minting_issuer = shared_issuer(&secret_path).unwrap();
        let token =
            minting_issuer.mint("com.example.demo", &caps(&["mavlink.read"]), TOKEN_TTL_SECONDS);

        // A separate issuer instance, reloaded from disk, must verify it.
        let verifying_issuer = shared_issuer(&secret_path).unwrap();
        let now = token.issued_at + 1;
        assert!(
            verifying_issuer.verify(&token, now).is_ok(),
            "token minted from the persisted secret must verify in a separate issuer"
        );
    }
}
