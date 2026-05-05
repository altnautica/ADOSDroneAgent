//! Cloudflare Tunnel orchestration.
//!
//! v0.1 stub — accepts the token, persists it to a root-owned 0600 file,
//! and returns a successful action result. Real cloudflared subprocess
//! lifecycle, init-unit drop, verify probe, and WebSocket log streaming
//! land in B7.7. The on-disk file location matches the Python reference
//! so a board can swap between agents without re-installing the tunnel.

use std::path::PathBuf;

use crate::models::CloudflareVerifyResponse;

const DEFAULT_TOKEN_PATH: &str = "/etc/ados/secrets/cloudflare-tunnel-token";

/// Resolve the path the Cloudflare Tunnel token gets persisted to.
/// `ADOS_CLOUDFLARE_TOKEN_PATH` overrides the default for tests + dev
/// containers that don't have /etc write access.
fn token_path() -> PathBuf {
    std::env::var_os("ADOS_CLOUDFLARE_TOKEN_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_TOKEN_PATH))
}

#[derive(Debug, thiserror::Error)]
pub enum CloudflareError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid token")]
    InvalidToken,
}

/// Persist the token (or token-bearing install command) to a root-owned
/// 0600 file. The actual cloudflared subprocess lifecycle lands in B7.7.
pub fn install_cloudflare_token(token_or_script: &str) -> Result<(), CloudflareError> {
    let token = extract_token(token_or_script).ok_or(CloudflareError::InvalidToken)?;
    let path = token_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp_path = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp_path, &token)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp_path, &path)?;
    tracing::info!("cloudflared token persisted; subprocess lifecycle in B7.7");
    Ok(())
}

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
    // Direct JWT shape — three base64url chunks separated by dots.
    if looks_like_jwt(trimmed) {
        return Some(trimmed.to_string());
    }
    // Pull out the longest JWT-shaped substring from the install command.
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

/// Probe the configured Cloudflare Tunnel public URL. v0.1 stub returns a
/// "no public URL configured yet" response. Real probe lands in B7.7
/// with reqwest + 5s timeout.
pub fn verify_tunnel(target_url: Option<&str>) -> CloudflareVerifyResponse {
    match target_url {
        Some(url) if !url.is_empty() => CloudflareVerifyResponse {
            reachable: false,
            status_code: None,
            latency_ms: None,
            target_url: Some(url.to_string()),
            error: Some("verify probe lands in B7.7".to_string()),
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
}
