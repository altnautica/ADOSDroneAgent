//! Download-URL allowlist for the cloud-relay plugin install path.
//!
//! A `signedUrl` comes from a cloud command-queue row. A compromised row could
//! redirect the agent to attacker-controlled HTTPS, plain HTTP, or a multi-GB
//! body. The allowlist runs before any byte of the body is trusted: scheme must
//! be `https`, host must end with an allowlisted suffix. Ports
//! `validate_download_url` + `CONVEX_HOST_SUFFIXES` + `DOWNLOAD_MAX_BYTES` from
//! `src/ados/plugins/remote_install_download.py`.

/// Hard cap on a downloaded archive body. Mirrors `DOWNLOAD_MAX_BYTES`.
pub const DOWNLOAD_MAX_BYTES: usize = 100 * 1024 * 1024;

/// Allowlisted hostname suffixes for a `signedUrl` download. Reject any host
/// not ending in one of these (suffix match on the labelled host, not a
/// substring search). Mirrors `CONVEX_HOST_SUFFIXES`.
pub const HOST_SUFFIXES: &[&str] = &[".convex.cloud", ".convex.altnautica.com", "localhost"];

/// A refused / aborted download. Distinct from install errors so the caller can
/// classify pre-signature transport failures separately. Mirrors `DownloadError`.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DownloadError {
    #[error("download url is empty")]
    Empty,
    #[error("download url is not parseable")]
    Unparseable,
    #[error("non-https url rejected")]
    NotHttps,
    #[error("download url has no host")]
    NoHost,
    #[error("download host {0:?} is not on the allowlist")]
    HostNotAllowed(String),
}

/// Reject URLs that escape the allowlist. Three checks in order: scheme is
/// `https`, host is present, host ends with an allowlisted suffix (`localhost`
/// is an exact match, not a suffix, so `evil.localhost.example.com` is refused).
/// Mirrors `validate_download_url`.
pub fn validate_download_url(url: &str) -> Result<(), DownloadError> {
    if url.is_empty() {
        return Err(DownloadError::Empty);
    }
    // Minimal scheme + host parse (no url crate dependency for one check):
    // split scheme, then the authority up to the first '/', '?', or '#'.
    let (scheme, rest) = match url.split_once("://") {
        Some((s, r)) => (s.to_ascii_lowercase(), r),
        None => return Err(DownloadError::Unparseable),
    };
    if scheme != "https" {
        return Err(DownloadError::NotHttps);
    }
    // Authority ends at the first path/query/fragment delimiter.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // Strip userinfo (`user@host`) and the port (`host:443`), then lowercase.
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    let host = host_port
        .split(':')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if host.is_empty() {
        return Err(DownloadError::NoHost);
    }
    if host == "localhost" {
        return Ok(());
    }
    for suffix in HOST_SUFFIXES {
        if *suffix == "localhost" {
            continue;
        }
        if host.ends_with(suffix) {
            return Ok(());
        }
    }
    Err(DownloadError::HostNotAllowed(host))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_convex_host_is_allowed() {
        assert!(validate_download_url("https://abc.convex.cloud/path?sig=1").is_ok());
        assert!(validate_download_url("https://self.convex.altnautica.com/x").is_ok());
        assert!(validate_download_url("https://localhost/x").is_ok());
        assert!(validate_download_url("https://localhost:8443/x").is_ok());
    }

    #[test]
    fn non_https_is_rejected() {
        assert_eq!(
            validate_download_url("http://abc.convex.cloud/x"),
            Err(DownloadError::NotHttps)
        );
    }

    #[test]
    fn empty_and_unparseable_rejected() {
        assert_eq!(validate_download_url(""), Err(DownloadError::Empty));
        assert_eq!(
            validate_download_url("not-a-url"),
            Err(DownloadError::Unparseable)
        );
    }

    #[test]
    fn off_allowlist_host_is_rejected() {
        assert_eq!(
            validate_download_url("https://evil.example.com/x"),
            Err(DownloadError::HostNotAllowed(
                "evil.example.com".to_string()
            ))
        );
        // localhost is an exact match, not a suffix: a lookalike is refused.
        assert_eq!(
            validate_download_url("https://evil.localhost.example.com/x"),
            Err(DownloadError::HostNotAllowed(
                "evil.localhost.example.com".to_string()
            ))
        );
    }

    #[test]
    fn userinfo_and_port_are_stripped_before_host_match() {
        assert!(validate_download_url("https://user@abc.convex.cloud:443/x").is_ok());
    }
}
