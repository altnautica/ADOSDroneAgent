//! Allowlisted HTTPS downloads for plugin archives and install-time payloads.
//!
//! Every plugin byte fetched over the network comes through here: a cloud-relay
//! `signedUrl`, a catalog or operator-supplied archive URL, and the payloads a
//! manifest pins. A hostile URL could otherwise point the agent at
//! attacker-controlled HTTPS, plain HTTP, or an endless body. The allowlist
//! runs before any byte is trusted: scheme must be `https`, host must end with
//! an allowlisted suffix. The URL is parsed with the same WHATWG parser the HTTP
//! client uses, so the host checked here is the host the request reaches (a `\`
//! in the authority, userinfo, percent-encoding and IDNA all resolve
//! identically on both sides), and every redirect hop is checked again.
//!
//! Two read shapes share one transport:
//! * [`fetch_capped`] reads a body into memory under a cap (an archive, at most
//!   [`DOWNLOAD_MAX_BYTES`]).
//! * [`fetch_to_file`] streams a body to disk under a cap while hashing it and
//!   refuses a digest mismatch (a payload, at most
//!   [`crate::manifest::PAYLOAD_MAX_BYTES`]), so a gigabyte binary is never
//!   held in memory and never lands under its final name unverified.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;

use sha2::{Digest, Sha256};

/// Hard cap on a downloaded archive body. The read aborts one byte past it, so
/// a larger body is never buffered whole.
pub const DOWNLOAD_MAX_BYTES: u64 = 100 * 1024 * 1024;

/// Allowlisted hostname suffixes. A host must end with one of these (a suffix
/// match on the parsed host, not a substring search); `localhost` is an exact
/// match. The convex hosts serve cloud-relay archives; the GitHub and object
/// storage hosts serve release archives and payloads.
pub const HOST_SUFFIXES: &[&str] = &[
    ".convex.cloud",
    ".convex.altnautica.com",
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
    ".amazonaws.com",
    "localhost",
];

/// Most redirect hops a download may follow. Each hop is allowlist-checked.
const DOWNLOAD_MAX_REDIRECTS: usize = 5;

/// How long one download may take end to end. Large enough for a payload over
/// a slow uplink; a stalled transfer still ends.
const DOWNLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1800);

/// A refused or failed download. Distinct from install errors so a caller can
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
    #[error("download body exceeds the {0}-byte cap")]
    TooLarge(u64),
    #[error("sha256 mismatch: expected {expected}, got {actual}")]
    Sha256Mismatch { expected: String, actual: String },
    #[error("download transport failed: {0}")]
    Transport(String),
    #[error("download write failed: {0}")]
    Io(String),
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
    let allowed = HOST_SUFFIXES.iter().any(|suffix| {
        if suffix.starts_with('.') {
            host.ends_with(suffix)
        } else {
            // A bare host is an exact match or a dot-separated parent, so
            // `github.com` admits `codeload.github.com` but not `evilgithub.com`.
            host == *suffix || (*suffix != "localhost" && host.ends_with(&format!(".{suffix}")))
        }
    });
    if allowed {
        Ok(())
    } else {
        Err(DownloadError::HostNotAllowed(host.to_string()))
    }
}

/// Verify a body against an expected SHA256 (case-insensitive hex). An empty
/// `expected` is a no-op: the caller declared no hash and the archive's Ed25519
/// check is the backstop.
pub fn verify_sha256(body: &[u8], expected: &str) -> Result<(), DownloadError> {
    if expected.is_empty() {
        return Ok(());
    }
    let actual = hex::encode(Sha256::digest(body));
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(DownloadError::Sha256Mismatch {
            expected: expected.to_string(),
            actual,
        })
    }
}

/// An open download: the body stream plus the length the server declared, if
/// it declared one.
pub struct DownloadBody {
    pub reader: Box<dyn Read + Send>,
    pub content_length: Option<u64>,
}

/// The transport seam. Production is [`HttpDownloadSource`]; tests hand back
/// bytes directly. `Send + Sync` so one source can be shared into a blocking
/// install task.
pub trait DownloadSource: Send + Sync {
    /// Open `url` for reading. The caller has validated it against the
    /// allowlist; an implementation that reaches the network checks again.
    fn open(&self, url: &str) -> Result<DownloadBody, DownloadError>;
}

/// Read a download into memory, refusing it once it passes `cap` bytes. Reads
/// at most `cap + 1` bytes, so an oversize body is never held whole.
pub fn fetch_capped(
    source: &dyn DownloadSource,
    url: &str,
    cap: u64,
) -> Result<Vec<u8>, DownloadError> {
    validate_download_url(url)?;
    let body = source.open(url)?;
    if body.content_length.is_some_and(|n| n > cap) {
        return Err(DownloadError::TooLarge(cap));
    }
    read_capped(body.reader, cap)
}

/// Stream a download to `dest`, hashing as it goes. The body is written to a
/// sibling temp file and renamed onto `dest` only when it is within `cap`, is
/// exactly `expected_size` bytes (when given), and hashes to `expected_sha256`;
/// on any failure the temp file is removed and `dest` is untouched.
pub fn fetch_to_file(
    source: &dyn DownloadSource,
    url: &str,
    dest: &Path,
    cap: u64,
    expected_size: Option<u64>,
    expected_sha256: &str,
) -> Result<(), DownloadError> {
    validate_download_url(url)?;
    let body = source.open(url)?;
    if body.content_length.is_some_and(|n| n > cap) {
        return Err(DownloadError::TooLarge(cap));
    }
    let parent = dest
        .parent()
        .ok_or_else(|| DownloadError::Io(format!("{} has no parent", dest.display())))?;
    std::fs::create_dir_all(parent).map_err(|e| DownloadError::Io(e.to_string()))?;
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = parent.join(format!(".{name}.part"));
    let result = stream_hashed(body.reader, &tmp, cap).and_then(|(written, actual)| {
        if let Some(size) = expected_size {
            if written != size {
                return Err(DownloadError::Io(format!(
                    "downloaded {written} bytes, the manifest pins {size}"
                )));
            }
        }
        if !actual.eq_ignore_ascii_case(expected_sha256) {
            return Err(DownloadError::Sha256Mismatch {
                expected: expected_sha256.to_string(),
                actual,
            });
        }
        std::fs::rename(&tmp, dest).map_err(|e| DownloadError::Io(e.to_string()))
    });
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Copy `reader` into a new file at `path`, at most `cap` bytes, returning the
/// byte count and the lowercase hex sha256 of what was written.
fn stream_hashed(
    reader: Box<dyn Read + Send>,
    path: &Path,
    cap: u64,
) -> Result<(u64, String), DownloadError> {
    let mut file = std::fs::File::create(path).map_err(|e| DownloadError::Io(e.to_string()))?;
    let mut limited = reader.take(cap + 1);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut written: u64 = 0;
    loop {
        let n = limited
            .read(&mut buf)
            .map_err(|e| DownloadError::Transport(e.to_string()))?;
        if n == 0 {
            break;
        }
        written += n as u64;
        if written > cap {
            return Err(DownloadError::TooLarge(cap));
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])
            .map_err(|e| DownloadError::Io(e.to_string()))?;
    }
    file.sync_all()
        .map_err(|e| DownloadError::Io(e.to_string()))?;
    Ok((written, hex::encode(hasher.finalize())))
}

/// Read `body` to the end, refusing it once it passes `cap` bytes. Reads at most
/// `cap + 1` bytes, so an oversize body is never held whole.
fn read_capped(body: impl Read, cap: u64) -> Result<Vec<u8>, DownloadError> {
    let mut out = Vec::new();
    body.take(cap + 1)
        .read_to_end(&mut out)
        .map_err(|e| DownloadError::Transport(e.to_string()))?;
    if out.len() as u64 > cap {
        return Err(DownloadError::TooLarge(cap));
    }
    Ok(out)
}

/// The live download source: a blocking HTTPS GET. Every redirect hop re-runs
/// the allowlist, and the caller's capped read stops one byte past its cap, so
/// a hostile URL can steer neither the destination nor the agent's memory. TLS
/// is rustls with the ring provider and the bundled webpki roots.
pub struct HttpDownloadSource {
    client: reqwest::blocking::Client,
}

impl HttpDownloadSource {
    pub fn new() -> Self {
        let policy = reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= DOWNLOAD_MAX_REDIRECTS {
                return attempt.error("too many redirects");
            }
            match validate_parsed_url(attempt.url()) {
                Ok(()) => attempt.follow(),
                Err(e) => attempt.error(e),
            }
        });
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("rustls accepts the default protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
        let client = reqwest::blocking::Client::builder()
            .use_preconfigured_tls(tls)
            .redirect(policy)
            .timeout(DOWNLOAD_TIMEOUT)
            .build()
            .expect("reqwest blocking client builds");
        HttpDownloadSource { client }
    }
}

impl Default for HttpDownloadSource {
    fn default() -> Self {
        Self::new()
    }
}

impl DownloadSource for HttpDownloadSource {
    fn open(&self, url: &str) -> Result<DownloadBody, DownloadError> {
        // Allowlist re-check at the transport boundary.
        validate_download_url(url)?;
        let resp = self
            .client
            .get(url)
            .send()
            .map_err(|e| DownloadError::Transport(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(DownloadError::Transport(format!(
                "http status {}",
                resp.status().as_u16()
            )));
        }
        let content_length = resp.content_length();
        Ok(DownloadBody {
            reader: Box::new(resp),
            content_length,
        })
    }
}

/// A source that serves fixed bodies by URL, for tests of the install paths.
/// Lives in the crate (not `#[cfg(test)]`) so the dependent crates' tests can
/// drive a download without a network.
#[derive(Default)]
pub struct StaticDownloadSource {
    bodies: std::collections::BTreeMap<String, Vec<u8>>,
}

impl StaticDownloadSource {
    /// Serve `body` for `url`.
    pub fn with(mut self, url: &str, body: Vec<u8>) -> Self {
        self.bodies.insert(url.to_string(), body);
        self
    }
}

impl DownloadSource for StaticDownloadSource {
    fn open(&self, url: &str) -> Result<DownloadBody, DownloadError> {
        let body = self
            .bodies
            .get(url)
            .cloned()
            .ok_or_else(|| DownloadError::Transport(format!("no body for {url}")))?;
        Ok(DownloadBody {
            content_length: Some(body.len() as u64),
            reader: Box::new(std::io::Cursor::new(body)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlisted_https_hosts_are_accepted() {
        for url in [
            "https://abc.convex.cloud/path?sig=1",
            "https://self.convex.altnautica.com/x",
            "https://localhost/x",
            "https://localhost:8443/x",
            "https://github.com/o/r/releases/download/v1/a.adosplug",
            "https://objects.githubusercontent.com/x",
            "https://release-assets.githubusercontent.com/x",
            "https://bucket.s3.amazonaws.com/x",
            "https://user@abc.convex.cloud:443/x",
        ] {
            assert!(validate_download_url(url).is_ok(), "{url}");
        }
    }

    #[test]
    fn non_https_empty_and_unparseable_are_rejected() {
        assert_eq!(
            validate_download_url("http://abc.convex.cloud/x"),
            Err(DownloadError::NotHttps)
        );
        assert_eq!(validate_download_url(""), Err(DownloadError::Empty));
        assert_eq!(
            validate_download_url("not-a-url"),
            Err(DownloadError::Unparseable)
        );
    }

    #[test]
    fn look_alike_hosts_are_rejected() {
        for (url, host) in [
            ("https://evil.example.com/x", "evil.example.com"),
            // localhost is an exact match, not a suffix.
            (
                "https://evil.localhost.example.com/x",
                "evil.localhost.example.com",
            ),
            // A bare suffix matches on a label boundary only.
            ("https://evilgithub.com/x", "evilgithub.com"),
            ("https://github.com.evil.io/x", "github.com.evil.io"),
            // The WHATWG parser treats `\` as `/` for https, so the request goes
            // to evil.example.com; the allowlist must see the same host.
            (
                "https://evil.example.com\\@x.convex.cloud/a",
                "evil.example.com",
            ),
        ] {
            assert_eq!(
                validate_download_url(url),
                Err(DownloadError::HostNotAllowed(host.to_string())),
                "{url}"
            );
        }
        assert!(matches!(
            validate_download_url("https://127.0.0.1/x"),
            Err(DownloadError::HostNotAllowed(_))
        ));
    }

    #[test]
    fn capped_read_stops_one_byte_past_the_cap_on_an_endless_body() {
        assert_eq!(
            read_capped(std::io::repeat(7), 16),
            Err(DownloadError::TooLarge(16))
        );
        assert_eq!(read_capped(&[1u8; 16][..], 16), Ok(vec![1u8; 16]));
    }

    #[test]
    fn verify_sha256_matches_case_insensitively() {
        let body = b"abc";
        let h = hex::encode(Sha256::digest(body));
        assert!(verify_sha256(body, &h).is_ok());
        assert!(verify_sha256(body, &h.to_uppercase()).is_ok());
        assert!(verify_sha256(body, "").is_ok());
        assert!(matches!(
            verify_sha256(body, &"00".repeat(32)),
            Err(DownloadError::Sha256Mismatch { .. })
        ));
    }

    #[test]
    fn a_streamed_file_lands_only_when_its_digest_matches() {
        let dir = tempfile::tempdir().unwrap();
        let url = "https://github.com/o/r/releases/download/v1/bin";
        let body = b"binary bytes".to_vec();
        let good = hex::encode(Sha256::digest(&body));
        let source = StaticDownloadSource::default().with(url, body.clone());
        let dest = dir.path().join("bin/aarch64-linux/tool");

        let err = fetch_to_file(&source, url, &dest, 1024, None, &"00".repeat(32)).unwrap_err();
        assert!(matches!(err, DownloadError::Sha256Mismatch { .. }), "{err}");
        assert!(!dest.exists(), "a mismatched body must not land");
        assert!(!dest.with_file_name(".tool.part").exists());

        let err = fetch_to_file(&source, url, &dest, 4, None, &good).unwrap_err();
        assert_eq!(err, DownloadError::TooLarge(4));
        assert!(!dest.exists());

        let err = fetch_to_file(&source, url, &dest, 1024, Some(3), &good).unwrap_err();
        assert!(matches!(err, DownloadError::Io(_)), "{err}");

        fetch_to_file(&source, url, &dest, 1024, Some(body.len() as u64), &good).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), body);
    }
}
