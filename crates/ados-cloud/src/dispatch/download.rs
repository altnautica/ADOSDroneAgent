//! Download-URL allowlist for the cloud-relay plugin install path.
//!
//! A `signedUrl` comes from a cloud command-queue row. A compromised row could
//! redirect the agent to attacker-controlled HTTPS, plain HTTP, or a multi-GB
//! body. The allowlist runs before any byte of the body is trusted: scheme must
//! be `https`, host must end with an allowlisted suffix. The URL is parsed with
//! the same WHATWG parser the HTTP client uses, so the host checked here is the
//! host the request reaches (a `\` in the authority, userinfo, percent-encoding
//! and IDNA all resolve identically on both sides).

/// Hard cap on a downloaded archive body. The live client aborts the read one
/// byte past it, so a larger body is never buffered whole.
pub const DOWNLOAD_MAX_BYTES: usize = 100 * 1024 * 1024;

/// Allowlisted hostname suffixes for a `signedUrl` download. Reject any host
/// not ending in one of these (suffix match on the labelled host, not a
/// substring search).
pub const HOST_SUFFIXES: &[&str] = &[".convex.cloud", ".convex.altnautica.com", "localhost"];

/// A refused / aborted download. Distinct from install errors so the caller can
/// classify pre-signature transport failures separately.
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
    #[error("download body exceeds the {DOWNLOAD_MAX_BYTES}-byte cap")]
    TooLarge,
    #[error("download transport failed")]
    Transport,
}

/// Reject URLs that escape the allowlist. Three checks in order: scheme is
/// `https`, host is present, host ends with an allowlisted suffix (`localhost`
/// is an exact match, not a suffix, so `evil.localhost.example.com` is refused).
pub fn validate_download_url(url: &str) -> Result<(), DownloadError> {
    if url.is_empty() {
        return Err(DownloadError::Empty);
    }
    let parsed = reqwest::Url::parse(url).map_err(|_| DownloadError::Unparseable)?;
    validate_parsed_url(&parsed)
}

/// The allowlist check on an already-parsed URL. The live client runs it again
/// on every redirect hop, so a permitted host cannot bounce the request to one
/// that is not.
pub fn validate_parsed_url(url: &reqwest::Url) -> Result<(), DownloadError> {
    if url.scheme() != "https" {
        return Err(DownloadError::NotHttps);
    }
    // The parser's normalised (lowercased, IDNA-mapped) host. An IP literal
    // never matches a suffix, so it falls through to the refusal.
    let host = url.host_str().unwrap_or("");
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
    Err(DownloadError::HostNotAllowed(host.to_string()))
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

    #[test]
    fn backslash_in_authority_resolves_to_the_host_the_client_reaches() {
        // The WHATWG parser treats `\` as `/` for https, so the request goes to
        // evil.example.com; the allowlist must see the same host.
        assert_eq!(
            validate_download_url("https://evil.example.com\\@x.convex.cloud/a"),
            Err(DownloadError::HostNotAllowed(
                "evil.example.com".to_string()
            ))
        );
    }

    #[test]
    fn ip_literal_host_is_rejected() {
        assert!(matches!(
            validate_download_url("https://127.0.0.1/x"),
            Err(DownloadError::HostNotAllowed(_))
        ));
    }
}
