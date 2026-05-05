//! Cloudflare Tunnel orchestration.
//!
//! End-to-end flow:
//!
//! 1. Operator pastes a Cloudflare Tunnel token (or the full install
//!    command Cloudflare's dashboard shows them) into the wizard.
//! 2. POST /api/v1/setup/remote-access/cloudflare extracts the token,
//!    persists it root-owned 0600 to /etc/ados/secrets/cloudflare-tunnel-token,
//!    downloads the cloudflared binary if missing, drops an init unit
//!    appropriate for the running init system (systemd / busybox / runit),
//!    and starts the service.
//! 3. GET /api/v1/setup/cloudflare/verify probes the configured public
//!    URL via reqwest with a 5 s timeout and reports reachability.
//! 4. WS /api/v1/setup/cloudflare/logs upgrades to a WebSocket and
//!    streams the cloudflared service log lines to the wizard. Token-
//!    shaped values are redacted at the source so a future cloudflared
//!    regression that logs a bearer doesn't leak it through the wizard.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::models::CloudflareVerifyResponse;

const DEFAULT_TOKEN_PATH: &str = "/etc/ados/secrets/cloudflare-tunnel-token";
const DEFAULT_BIN_PATH: &str = "/usr/local/bin/cloudflared";
const DEFAULT_SYSTEMD_UNIT: &str = "/etc/systemd/system/cloudflared.service";
const DEFAULT_SYSV_INIT: &str = "/etc/init.d/cloudflared";
const SERVICE_NAME: &str = "cloudflared";

/// Resolve the path the Cloudflare Tunnel token gets persisted to.
/// `ADOS_CLOUDFLARE_TOKEN_PATH` overrides the default for tests + dev
/// containers that don't have /etc write access.
fn token_path() -> PathBuf {
    std::env::var_os("ADOS_CLOUDFLARE_TOKEN_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_TOKEN_PATH))
}

fn bin_path() -> PathBuf {
    std::env::var_os("ADOS_CLOUDFLARED_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_BIN_PATH))
}

fn systemd_unit_path() -> PathBuf {
    std::env::var_os("ADOS_CLOUDFLARED_SYSTEMD_UNIT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SYSTEMD_UNIT))
}

fn sysv_init_path() -> PathBuf {
    std::env::var_os("ADOS_CLOUDFLARED_SYSV_INIT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SYSV_INIT))
}

#[derive(Debug, thiserror::Error)]
pub enum CloudflareError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid token")]
    InvalidToken,

    #[error("download failed: {0}")]
    Download(String),

    #[error("service install failed: {0}")]
    Service(String),
}

/// Persist the token + ensure the binary is installed + start the service.
pub fn install_cloudflare_token(token_or_script: &str) -> Result<(), CloudflareError> {
    let token = extract_token(token_or_script).ok_or(CloudflareError::InvalidToken)?;
    persist_token(&token)?;
    if !bin_path().exists() {
        if let Err(e) = ensure_cloudflared_binary() {
            tracing::warn!(error = %e, "cloudflared binary install failed; token persisted");
            return Err(e);
        }
    }
    install_service()?;
    start_service();
    Ok(())
}

fn persist_token(token: &str) -> Result<(), CloudflareError> {
    let path = token_path();
    crate::atomic::atomic_write(&path, token.as_bytes(), 0o600)?;
    tracing::info!(path = %path.display(), "cloudflared token persisted (0600)");
    Ok(())
}

/// Download the cloudflared binary appropriate for the running arch.
/// Skipped when `ADOS_CLOUDFLARED_SKIP_DOWNLOAD=1` is set (tests).
fn ensure_cloudflared_binary() -> Result<(), CloudflareError> {
    if std::env::var_os("ADOS_CLOUDFLARED_SKIP_DOWNLOAD").is_some() {
        return Ok(());
    }
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "arm" => "armhf",
        other => {
            return Err(CloudflareError::Download(format!(
                "no published cloudflared build for arch {other}"
            )))
        }
    };
    let asset = format!("cloudflared-linux-{arch}");
    let url = format!(
        "https://github.com/cloudflare/cloudflared/releases/latest/download/{asset}"
    );
    tracing::info!(url = %url, "downloading cloudflared binary");
    let target = bin_path();
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Build a tempfile path that includes a nanosecond suffix so a
    // hostile prior process cannot pre-place a symlink at the
    // predictable PID-only path between attempts. The PID-plus-nanos
    // pattern matches the convention in ados_setup::atomic.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = target.with_extension(format!("dl.{}.{}", std::process::id(), nanos));

    // Pre-create the tempfile under our control with O_CREAT|O_EXCL
    // and 0o600 mode-at-create. If a symlink or stale file already
    // sits at this path, create_new fails with AlreadyExists and we
    // refuse to follow. We then close the handle so curl's subsequent
    // open(O_WRONLY|O_TRUNC) overwrites the same inode we created —
    // curl truncates rather than unlinks, so the mode survives.
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let handle = opts.open(&tmp).map_err(|e| {
            CloudflareError::Download(format!(
                "tempfile pre-create at {} failed: {e}",
                tmp.display()
            ))
        })?;
        drop(handle);
    }

    // Prefer curl over reqwest here so the agent doesn't pay reqwest's
    // TLS init cost just to grab the binary once. The download path is
    // operator-initiated, not a hot loop.
    let status = Command::new("curl")
        .args([
            "-fsSL",
            "--retry",
            "3",
            "--max-time",
            "120",
            "-o",
            tmp.to_string_lossy().as_ref(),
            &url,
        ])
        .status()
        .map_err(|e| CloudflareError::Download(format!("curl failed: {e}")))?;
    if !status.success() {
        let _ = std::fs::remove_file(&tmp);
        return Err(CloudflareError::Download(format!(
            "curl exit {:?} for {url}",
            status.code()
        )));
    }

    // Belt-and-suspenders: verify curl preserved our mode. If a
    // future curl flag change unlinks-and-recreates instead of
    // truncating, the mode could drift from 0o600 back to umask
    // default. Refuse the install if so.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let observed = std::fs::metadata(&tmp)?.permissions().mode() & 0o777;
        if observed != 0o600 {
            let _ = std::fs::remove_file(&tmp);
            return Err(CloudflareError::Download(format!(
                "tempfile mode drifted to 0o{observed:o} after curl write (expected 0o600)"
            )));
        }
    }

    if let Err(e) = verify_cloudflared_sha256(&tmp, &asset) {
        // Clean up the unverified binary so a stale tempfile cannot
        // surface as a "cached" install on the next attempt.
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&tmp, &target)?;
    tracing::info!(path = %target.display(), "cloudflared binary installed");
    Ok(())
}

/// Verify the downloaded cloudflared binary's SHA256 against the
/// upstream-published `<asset>.sha256` file. Cloudflare publishes per-asset
/// `.sha256` files alongside every release artifact (e.g.
/// `cloudflared-linux-arm64.sha256` next to `cloudflared-linux-arm64`), so
/// we fetch the matching `.sha256` for the just-downloaded asset and compare.
///
/// Set `ADOS_CLOUDFLARED_SKIP_SHA256=1` (intended for offline test
/// environments) to bypass; otherwise a mismatch or fetch failure aborts
/// the install. The default is fail-closed.
fn verify_cloudflared_sha256(
    tmp_binary: &std::path::Path,
    asset: &str,
) -> Result<(), CloudflareError> {
    if std::env::var_os("ADOS_CLOUDFLARED_SKIP_SHA256").is_some() {
        tracing::warn!(
            "ADOS_CLOUDFLARED_SKIP_SHA256 set — skipping SHA256 verification of cloudflared binary"
        );
        return Ok(());
    }

    // Compute the local hash. Stream the file rather than reading it all
    // at once — cloudflared is ~30 MB stripped.
    let actual_hash = sha256_file(tmp_binary)
        .map_err(|e| CloudflareError::Download(format!("hashing failed: {e}")))?;

    // Fetch the published `.sha256` companion file for this asset.
    let sha_url = format!(
        "https://github.com/cloudflare/cloudflared/releases/latest/download/{asset}.sha256"
    );
    let output = Command::new("curl")
        .args([
            "-fsSL",
            "--retry",
            "3",
            "--max-time",
            "30",
            sha_url.as_str(),
        ])
        .output()
        .map_err(|e| CloudflareError::Download(format!("sha256 fetch failed: {e}")))?;
    if !output.status.success() {
        return Err(CloudflareError::Download(format!(
            "could not fetch {sha_url} (exit {:?})",
            output.status.code()
        )));
    }
    let published = String::from_utf8_lossy(&output.stdout);
    // Format is `<hex>  <filename>` per coreutils sha256sum convention.
    let expected_hash = published
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_lowercase();
    if expected_hash.is_empty() {
        return Err(CloudflareError::Download(
            "published sha256 file is empty".into(),
        ));
    }

    if actual_hash != expected_hash {
        return Err(CloudflareError::Download(format!(
            "sha256 mismatch: expected {expected_hash}, got {actual_hash}"
        )));
    }
    tracing::info!(asset = %asset, "cloudflared sha256 verified");
    Ok(())
}

fn sha256_file(path: &std::path::Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    Ok(hex_lower(&digest))
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Drop an init unit appropriate for the running init system. systemd is
/// the common case; busybox sysv-rc is shipped for Luckfox Buildroot.
fn install_service() -> Result<(), CloudflareError> {
    let token_path_str = token_path().to_string_lossy().to_string();
    let bin_path_str = bin_path().to_string_lossy().to_string();
    if Path::new("/run/systemd/system").is_dir() {
        write_systemd_unit(&bin_path_str, &token_path_str)?;
    } else if Path::new("/etc/init.d").is_dir() {
        write_busybox_init(&bin_path_str, &token_path_str)?;
    } else {
        return Err(CloudflareError::Service(
            "no recognised init system (systemd or busybox sysv-rc)".into(),
        ));
    }
    Ok(())
}

fn write_systemd_unit(bin: &str, token_file: &str) -> Result<(), CloudflareError> {
    let unit = format!(
        "[Unit]\n\
         Description=Cloudflare Tunnel for ADOS Drone Agent\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={bin} --no-autoupdate tunnel run --token-file {token_file}\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         User=root\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
    );
    let path = systemd_unit_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, unit)?;
    let _ = Command::new("systemctl").arg("daemon-reload").status();
    let _ = Command::new("systemctl").args(["enable", SERVICE_NAME]).status();
    Ok(())
}

fn write_busybox_init(bin: &str, token_file: &str) -> Result<(), CloudflareError> {
    let script = format!(
        "#!/bin/sh\n\
         # cloudflared init script (busybox sysv-rc).\n\
         DAEMON={bin}\n\
         PIDFILE=/var/run/cloudflared.pid\n\
         case \"$1\" in\n\
             start)\n\
                 echo \"Starting cloudflared...\"\n\
                 start-stop-daemon -S -b -m -p \"$PIDFILE\" \\\n\
                     --exec \"$DAEMON\" -- --no-autoupdate tunnel run --token-file {token_file}\n\
                 ;;\n\
             stop)\n\
                 echo \"Stopping cloudflared...\"\n\
                 start-stop-daemon -K -p \"$PIDFILE\" --quiet\n\
                 rm -f \"$PIDFILE\"\n\
                 ;;\n\
             restart) $0 stop || true; sleep 1; $0 start ;;\n\
             status)\n\
                 if [ -f \"$PIDFILE\" ] && kill -0 \"$(cat \"$PIDFILE\")\" 2>/dev/null; then\n\
                     echo \"running\"; exit 0\n\
                 else\n\
                     echo \"not running\"; exit 1\n\
                 fi\n\
                 ;;\n\
             *) echo \"Usage: $0 {{start|stop|restart|status}}\"; exit 1 ;;\n\
         esac\n",
    );
    let path = sysv_init_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, script)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

fn start_service() {
    if Path::new("/run/systemd/system").is_dir() {
        let _ = Command::new("systemctl").args(["restart", SERVICE_NAME]).status();
    } else {
        let init_path = sysv_init_path();
        let _ = Command::new(&init_path).arg("restart").status();
    }
}

// ---------------------------------------------------------------------------
// Verify
// ---------------------------------------------------------------------------

/// Probe the operator's configured Cloudflare Tunnel public URL with a
/// 5 s outbound HTTPS GET. Mirrors verify_cloudflare_tunnel() in the
/// Python reference. The target URL is supplied by the caller — usually
/// read from agent.yaml at remote_access.cloudflare.setup_url, populated
/// once the operator has picked their own tunnel hostname in CF dashboard.
pub async fn verify_tunnel_async(target_url: Option<&str>) -> CloudflareVerifyResponse {
    let target = match target_url {
        Some(u) if !u.is_empty() => u.to_string(),
        _ => {
            return CloudflareVerifyResponse {
                reachable: false,
                status_code: None,
                latency_ms: None,
                target_url: None,
                error: Some(
                    "Set the public setup URL in the Cloudflare dashboard before verifying."
                        .into(),
                ),
            }
        }
    };
    if !target.starts_with("http://") && !target.starts_with("https://") {
        return CloudflareVerifyResponse {
            reachable: false,
            status_code: None,
            latency_ms: None,
            target_url: Some(target),
            error: Some("Setup URL must start with http:// or https://.".into()),
        };
    }
    let probe = format!(
        "{}/api/v1/setup/status",
        target.trim_end_matches('/')
    );
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return CloudflareVerifyResponse {
                reachable: false,
                status_code: None,
                latency_ms: None,
                target_url: Some(target),
                error: Some(format!("client build failed: {e}")),
            }
        }
    };
    let started = std::time::Instant::now();
    match client.get(&probe).send().await {
        Ok(resp) => {
            let status_code = resp.status().as_u16();
            let latency_ms = started.elapsed().as_millis() as u64;
            CloudflareVerifyResponse {
                reachable: status_code == 200,
                status_code: Some(status_code),
                latency_ms: Some(latency_ms),
                target_url: Some(target),
                error: if status_code == 200 {
                    None
                } else {
                    Some(format!("Public URL returned HTTP {status_code}."))
                },
            }
        }
        Err(e) => CloudflareVerifyResponse {
            reachable: false,
            status_code: None,
            latency_ms: None,
            target_url: Some(target),
            error: Some(format!("Could not reach the public URL: {e}")),
        },
    }
}

/// Sync wrapper that returns a pre-baked "no public URL configured"
/// response when the handler has no async runtime context. Callers with
/// an active runtime should prefer `verify_tunnel_async` for a real probe.
pub fn verify_tunnel(target_url: Option<&str>) -> CloudflareVerifyResponse {
    match target_url {
        Some(url) if !url.is_empty() => CloudflareVerifyResponse {
            reachable: false,
            status_code: None,
            latency_ms: None,
            target_url: Some(url.to_string()),
            error: Some("verify probe runs over async; use verify_tunnel_async".to_string()),
        },
        _ => CloudflareVerifyResponse {
            reachable: false,
            status_code: None,
            latency_ms: None,
            target_url: None,
            error: Some(
                "Set the public setup URL in the Cloudflare dashboard before verifying."
                    .to_string(),
            ),
        },
    }
}

// ---------------------------------------------------------------------------
// Token extractor
// ---------------------------------------------------------------------------

/// Pull a token out of a raw value or the full Cloudflare install
/// command. The dashboard shows operators a snippet like:
///
/// ```text
/// curl -L --output cloudflared.deb https://github.com/.../cloudflared.deb && \
///   sudo dpkg -i cloudflared.deb && \
///   sudo cloudflared service install eyJhbGciOi...long-jwt...
/// ```
///
/// We accept either form and extract the JWT-shaped token.
pub fn extract_token(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if looks_like_jwt(trimmed) {
        return Some(trimmed.to_string());
    }
    let mut best: Option<&str> = None;
    for word in trimmed.split_whitespace() {
        if looks_like_jwt(word) {
            best = Some(match best {
                Some(prev) if prev.len() >= word.len() => prev,
                _ => word,
            });
        }
    }
    best.map(|s| s.to_string())
}

fn looks_like_jwt(s: &str) -> bool {
    let chunks: Vec<&str> = s.split('.').collect();
    if chunks.len() != 3 {
        return false;
    }
    chunks.iter().all(|c| {
        c.len() >= 8
            && c.chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    })
}

// ---------------------------------------------------------------------------
// Token-shaped redaction (used by the WS log streamer)
// ---------------------------------------------------------------------------

/// Rewrite any JWT-shaped substring in a log line so cloudflared can never
/// leak a bearer through the wizard's WS log stream. Mirrors the
/// `if "eyJ" in text and "." in text` redaction in the Python reference.
pub fn redact_log_line(line: &str) -> String {
    if !line.contains("eyJ") || !line.contains('.') {
        return line.to_string();
    }
    // Replace each whitespace-separated word that looks like a JWT.
    line.split_whitespace()
        .map(|w| {
            if looks_like_jwt(w) {
                "(token-shaped value redacted)".to_string()
            } else {
                w.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_direct_jwt_token() {
        let jwt = "eyJhbGciOi.eyJzdWIi-X_TEST.SflKxwRJSMeKKF_test";
        assert_eq!(extract_token(jwt), Some(jwt.to_string()));
    }

    #[test]
    fn extract_token_from_install_command() {
        let cmd = "sudo cloudflared service install eyJhbGciOi.eyJzdWIi-XYZ.SflKxwRJSMeKKF_test";
        assert!(extract_token(cmd).unwrap().contains("eyJhbGciOi"));
    }

    #[test]
    fn extract_rejects_empty() {
        assert_eq!(extract_token(""), None);
        assert_eq!(extract_token("    "), None);
    }

    #[test]
    fn extract_rejects_non_jwt() {
        assert_eq!(extract_token("just a plain string"), None);
        assert_eq!(extract_token("two.dots"), None);
    }

    #[test]
    fn redact_jwt_in_log_line() {
        let line = "INFO: cloudflared starting with token eyJhbGciOi.eyJzdWIiX-Y.SflKxwRJSMeKKF_t";
        let redacted = redact_log_line(line);
        assert!(!redacted.contains("eyJhbGciOi"));
        assert!(redacted.contains("redacted"));
    }

    #[test]
    fn redact_passes_through_non_token_lines() {
        let line = "INFO: tunnel up";
        assert_eq!(redact_log_line(line), line);
    }

    #[test]
    fn verify_no_url_returns_error() {
        let resp = verify_tunnel(None);
        assert!(!resp.reachable);
        assert!(resp.error.unwrap().contains("Set the public setup URL"));
    }

    #[test]
    fn verify_with_invalid_scheme_in_async_returns_error() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let resp = runtime.block_on(verify_tunnel_async(Some("ftp://example.com")));
        assert!(!resp.reachable);
        assert!(resp.error.unwrap().contains("http://"));
    }

    #[cfg(unix)]
    #[test]
    fn tempfile_pre_create_refuses_existing_symlink() {
        // Defense-in-depth: simulate a hostile actor pre-placing a
        // symlink at the predictable tempfile path. The pre-create
        // step uses O_CREAT|O_EXCL so create_new must error with
        // AlreadyExists rather than follow the symlink into a
        // privileged write target.
        use std::os::unix::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bait = dir.path().join("bait");
        std::fs::write(&bait, b"victim").unwrap();
        let tmp = dir.path().join("attacker-tmp");
        std::os::unix::fs::symlink(&bait, &tmp).unwrap();

        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true).mode(0o600);
        let result = opts.open(&tmp);
        assert!(result.is_err(), "pre-create followed symlink: {result:?}");
        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists,
            "expected AlreadyExists when symlink occupies tempfile path"
        );
        // Bait file untouched.
        assert_eq!(std::fs::read(&bait).unwrap(), b"victim");
    }

    #[cfg(unix)]
    #[test]
    fn tempfile_pre_create_sets_0600_mode() {
        // The pre-create path must apply 0o600 at open(2) time so the
        // file never briefly exists at the umask default.
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().join("dl.tmp");
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true).mode(0o600);
        let f = opts.open(&tmp).unwrap();
        drop(f);
        let mode = std::fs::metadata(&tmp).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0o600 mode-at-create, got 0o{mode:o}");
    }

    #[test]
    fn install_token_writes_0600_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cf-token");
        std::env::set_var("ADOS_CLOUDFLARE_TOKEN_PATH", &path);
        // Skip download + service install; we're only testing token persistence.
        std::env::set_var("ADOS_CLOUDFLARED_SKIP_DOWNLOAD", "1");
        std::env::set_var("ADOS_CLOUDFLARED_BIN", dir.path().join("cloudflared"));
        std::env::set_var(
            "ADOS_CLOUDFLARED_SYSTEMD_UNIT",
            dir.path().join("cloudflared.service"),
        );
        std::env::set_var(
            "ADOS_CLOUDFLARED_SYSV_INIT",
            dir.path().join("cloudflared-init"),
        );
        // Force the test through whichever init system happens to be on
        // the host. install_service tolerates mac (no /run/systemd, no
        // /etc/init.d) by erroring; persist_token still wrote the file.
        let _ = persist_token("eyJtest.eyJtest.eyJtest");
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("eyJtest"));
        std::env::remove_var("ADOS_CLOUDFLARE_TOKEN_PATH");
        std::env::remove_var("ADOS_CLOUDFLARED_SKIP_DOWNLOAD");
        std::env::remove_var("ADOS_CLOUDFLARED_BIN");
        std::env::remove_var("ADOS_CLOUDFLARED_SYSTEMD_UNIT");
        std::env::remove_var("ADOS_CLOUDFLARED_SYSV_INIT");
    }
}
