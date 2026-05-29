//! GitHub Releases update checker.
//!
//! Ports `src/ados/services/ota/checker.py`: list `/repos/{repo}/releases`, ETag
//! cache, the `^v\d+\.\d+\.\d+$` full-agent tag filter, newest-first eligible
//! pick (skip drafts; skip prereleases on the stable channel; skip non
//! full-agent tags), strict-semver version compare, and the wheel + SHA256SUMS
//! asset selection.
//!
//! The HTTP fetch is isolated behind [`ReleaseSource`] so the release-pick and
//! version-compare logic is unit-tested without a network. The production
//! source is [`GithubSource`] (sync `ureq` over HTTPS) — the poll is a oneshot
//! the daily loop drives, so a sync client is the right tool.

use serde::Deserialize;

/// GitHub API base. Mirrors the Python `GITHUB_API`. The live releases source
/// (the transport shim) targets `{GITHUB_API}/repos/{repo}/releases`.
pub const GITHUB_API: &str = "https://api.github.com";

/// Full-agent release tag pattern. Mirrors the Python `_FULL_AGENT_TAG`. Other
/// tag lines that ship from the same repo (e.g. image builds) carry their own
/// prefixes and must not be considered upgrade candidates for the full agent.
pub const FULL_AGENT_TAG_RE: &str = r"^v\d+\.\d+\.\d+$";

/// Parse a strict-semver string into a comparable tuple. Strips a leading `v`,
/// splits on `.`, parses each segment as a non-negative integer. Returns `None`
/// if any segment is not an integer — callers treat `None` as a hard failure,
/// never a silent zero. Mirrors the Python `_version_tuple`.
pub fn version_tuple(version: &str) -> Option<Vec<u64>> {
    let trimmed = version.strip_prefix('v').unwrap_or(version);
    let mut parts = Vec::new();
    for segment in trimmed.split('.') {
        match segment.parse::<u64>() {
            Ok(n) => parts.push(n),
            Err(_) => return None,
        }
    }
    Some(parts)
}

/// Whether a tag matches the full-agent pattern `v<MAJOR>.<MINOR>.<PATCH>`.
/// Implemented without a regex crate (the workspace carries none): exactly a
/// leading `v` then three dot-separated all-digit, non-empty segments.
fn is_full_agent_tag(tag: &str) -> bool {
    let Some(rest) = tag.strip_prefix('v') else {
        return false;
    };
    let segments: Vec<&str> = rest.split('.').collect();
    if segments.len() != 3 {
        return false;
    }
    segments
        .iter()
        .all(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
}

/// One release asset from the GitHub Releases payload. Only the fields the
/// checker reads are typed; everything else is ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct ReleaseAsset {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub browser_download_url: String,
    #[serde(default)]
    pub size: u64,
}

/// One release from the GitHub Releases list. Only the fields the checker reads
/// are typed.
#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    #[serde(default)]
    pub tag_name: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub assets: Vec<ReleaseAsset>,
    #[serde(default)]
    pub published_at: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub html_url: String,
}

/// The resolved available-update record. Mirrors the Python `UpdateManifest`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateManifest {
    pub version: String,
    pub channel: String,
    pub published_at: String,
    pub download_url: String,
    pub file_size: u64,
    pub sha256: String,
    pub changelog: String,
    pub release_url: String,
}

/// Update poller config. Mirrors the fields of the Python `OtaConfig` the
/// checker reads (`channel`, `github_repo`); the default repo matches the Python
/// config default so a poll behaves identically.
#[derive(Debug, Clone)]
pub struct UpdateConfig {
    pub channel: String,
    pub github_repo: String,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        UpdateConfig {
            channel: "stable".to_string(),
            github_repo: "altnautica/ADOSDroneAgent".to_string(),
        }
    }
}

/// Outcome of one releases fetch, mirroring the Python status-code branches.
pub enum FetchOutcome {
    /// 304: ETag cache hit, nothing changed since the last fetch.
    NotModified,
    /// 403: rate limited; skip this check.
    RateLimited,
    /// 200: the release list plus the new ETag (empty when the header is absent).
    Ok {
        releases: Vec<Release>,
        etag: String,
    },
    /// A transport / decode error; treated as "no update" by the checker.
    Error(String),
}

/// The releases-list source. Production is [`GithubSource`]; tests inject a
/// fake so the pick + compare logic runs without a network.
pub trait ReleaseSource {
    /// Fetch the releases list, passing the prior ETag for conditional GET.
    fn fetch_releases(&self, repo: &str, etag: &str) -> FetchOutcome;
    /// Fetch the `SHA256SUMS` body for digest extraction.
    fn fetch_sha256sums(&self, url: &str) -> Option<String>;
}

/// Extract the digest for `wheel_name` from a `SHA256SUMS` body. Each line is
/// `<hex>  <name>` or `<hex> *<name>`; the `*` (binary marker) is stripped from
/// the name before the compare. Mirrors the Python `_fetch_sha256` parse.
pub fn sha256_for_wheel(sums_body: &str, wheel_name: &str) -> Option<String> {
    for line in sums_body.trim().lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 && parts[1].trim_start_matches('*') == wheel_name {
            return Some(parts[0].to_string());
        }
    }
    None
}

/// The update checker. Holds the ETag + the last resolved manifest across polls
/// so a 304 can still surface a newer cached manifest, exactly like the Python
/// `UpdateChecker`.
pub struct UpdateChecker<S: ReleaseSource> {
    config: UpdateConfig,
    source: S,
    etag: String,
    cached_manifest: Option<UpdateManifest>,
    last_manifest: Option<UpdateManifest>,
}

impl<S: ReleaseSource> UpdateChecker<S> {
    pub fn new(config: UpdateConfig, source: S) -> Self {
        UpdateChecker {
            config,
            source,
            etag: String::new(),
            cached_manifest: None,
            last_manifest: None,
        }
    }

    /// The last manifest resolved by a successful poll (the GCS reads it).
    pub fn last_manifest(&self) -> Option<&UpdateManifest> {
        self.last_manifest.as_ref()
    }

    /// Poll once and return the manifest when a strictly-newer release is
    /// available, else `None`. Mirrors `check_for_update`:
    /// * 304 → return the cached manifest only if it is newer than current.
    /// * 403 / transport error → `None`.
    /// * 200 → pick the newest eligible release; return it only when its version
    ///   is strictly greater than `current_version`.
    pub fn check_for_update(&mut self, current_version: &str) -> Option<UpdateManifest> {
        let outcome = self
            .source
            .fetch_releases(&self.config.github_repo, &self.etag);
        let (releases, new_etag) = match outcome {
            FetchOutcome::NotModified => {
                // Cache hit: surface the cached manifest only if it is newer.
                let cached = self.cached_manifest.clone()?;
                let cached_t = version_tuple(&cached.version)?;
                let current_t = version_tuple(current_version)?;
                return if cached_t > current_t {
                    Some(cached)
                } else {
                    None
                };
            }
            FetchOutcome::RateLimited => {
                tracing::warn!("github rate limited; skipping update check");
                return None;
            }
            FetchOutcome::Error(e) => {
                tracing::warn!(error = %e, "update check failed");
                return None;
            }
            FetchOutcome::Ok { releases, etag } => (releases, etag),
        };
        if !new_etag.is_empty() {
            self.etag = new_etag;
        }

        // Newest-first eligible pick: skip drafts; on stable skip prereleases;
        // require a full-agent tag.
        let stable = self.config.channel == "stable";
        let release = releases.iter().find(|r| {
            if r.draft {
                return false;
            }
            if stable && r.prerelease {
                return false;
            }
            is_full_agent_tag(&r.tag_name)
        })?;

        let release_version = release.tag_name.trim_start_matches('v').to_string();
        if release_version.is_empty() {
            tracing::warn!("release missing tag");
            return None;
        }
        let release_t = version_tuple(&release_version)?;
        let current_t = version_tuple(current_version)?;
        if release_t <= current_t {
            tracing::info!(
                latest = %release_version,
                current = %current_version,
                "no update available"
            );
            return None;
        }

        // Select the wheel + SHA256SUMS assets.
        let mut wheel: Option<&ReleaseAsset> = None;
        let mut sha_asset: Option<&ReleaseAsset> = None;
        for asset in &release.assets {
            if asset.name.ends_with(".whl") {
                wheel = Some(asset);
            } else if asset.name == "SHA256SUMS" {
                sha_asset = Some(asset);
            }
        }
        let wheel = match wheel {
            Some(w) => w,
            None => {
                tracing::warn!(version = %release_version, "no wheel asset");
                return None;
            }
        };

        let sha256_hex = sha_asset
            .filter(|a| !a.browser_download_url.is_empty())
            .and_then(|a| self.source.fetch_sha256sums(&a.browser_download_url))
            .and_then(|body| sha256_for_wheel(&body, &wheel.name))
            .unwrap_or_default();
        if sha256_hex.is_empty() {
            tracing::warn!(
                version = %release_version,
                "no sha256 available; update will skip hash verification"
            );
        }

        let manifest = UpdateManifest {
            version: release_version,
            channel: self.config.channel.clone(),
            published_at: release.published_at.clone(),
            download_url: wheel.browser_download_url.clone(),
            file_size: wheel.size,
            sha256: sha256_hex,
            changelog: release.body.clone(),
            release_url: release.html_url.clone(),
        };
        tracing::info!(version = %manifest.version, "update available");
        self.last_manifest = Some(manifest.clone());
        self.cached_manifest = Some(manifest.clone());
        Some(manifest)
    }
}

/// The live releases source: a synchronous HTTPS client to `GITHUB_API` on the
/// pure-Rust rustls path. A thin transport shim over the already-verified
/// release-pick / version-compare / SHA256SUMS-parse logic above. The poll is a
/// oneshot the daily loop drives, so a blocking client is the right tool.
///
/// TLS is the RustCrypto-backed rustls config from [`crate::tls`] (no ring), so
/// this links into the same C-toolchain-free static binary as the rest of the
/// relay.
pub struct GithubSource {
    client: reqwest::blocking::Client,
}

impl GithubSource {
    /// Build a source with the shared RustCrypto rustls config and the timeouts
    /// the Python checker used (30 s for the releases list).
    pub fn new() -> Self {
        let client = reqwest::blocking::Client::builder()
            .use_preconfigured_tls(crate::tls::client_config())
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("reqwest blocking client builds with the rustls config");
        GithubSource { client }
    }
}

impl Default for GithubSource {
    fn default() -> Self {
        Self::new()
    }
}

impl ReleaseSource for GithubSource {
    fn fetch_releases(&self, repo: &str, etag: &str) -> FetchOutcome {
        let url = format!("{GITHUB_API}/repos/{repo}/releases");
        let mut req = self
            .client
            .get(&url)
            .header("Accept", "application/vnd.github+json")
            // GitHub requires a User-Agent or it 403s.
            .header("User-Agent", "ados-agent");
        if !etag.is_empty() {
            req = req.header("If-None-Match", etag);
        }
        let resp = match req.send() {
            Ok(r) => r,
            Err(e) => return FetchOutcome::Error(format!("transport: {e}")),
        };
        let status = resp.status();
        // The two codes the Python checker special-cases.
        if status.as_u16() == 304 {
            return FetchOutcome::NotModified;
        }
        if status.as_u16() == 403 {
            return FetchOutcome::RateLimited;
        }
        if !status.is_success() {
            return FetchOutcome::Error(format!("github status {}", status.as_u16()));
        }
        let new_etag = resp
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        match resp.json::<Vec<Release>>() {
            Ok(releases) => FetchOutcome::Ok {
                releases,
                etag: new_etag,
            },
            Err(e) => FetchOutcome::Error(format!("releases decode: {e}")),
        }
    }

    fn fetch_sha256sums(&self, url: &str) -> Option<String> {
        let resp = self
            .client
            .get(url)
            .header("User-Agent", "ados-agent")
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.text().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- version_tuple parity ----

    #[test]
    fn version_tuple_strips_v_and_parses() {
        assert_eq!(version_tuple("v0.49.3"), Some(vec![0, 49, 3]));
        assert_eq!(version_tuple("0.49.3"), Some(vec![0, 49, 3]));
        assert_eq!(version_tuple("1.0.0"), Some(vec![1, 0, 0]));
        // Non-integer segment → None (hard failure, never a silent zero).
        assert_eq!(version_tuple("0.49.x"), None);
        assert_eq!(version_tuple("v0.49.3-rc1"), None);
    }

    #[test]
    fn version_compare_orders_correctly() {
        assert!(version_tuple("0.49.4") > version_tuple("0.49.3"));
        assert!(version_tuple("0.50.0") > version_tuple("0.49.99"));
        assert!(version_tuple("1.0.0") > version_tuple("0.49.3"));
        assert!(version_tuple("0.49.3") == version_tuple("0.49.3"));
    }

    // ---- tag filter ----

    #[test]
    fn full_agent_tag_filter_matches_only_strict_tags() {
        assert!(is_full_agent_tag("v0.49.3"));
        assert!(is_full_agent_tag("v1.0.0"));
        assert!(is_full_agent_tag("v10.20.30"));
        // Rejected: image/lite prefixes, pre-release, missing v, wrong segment count.
        assert!(!is_full_agent_tag("lite-v0.49.3"));
        assert!(!is_full_agent_tag("lite-image-v0.49.3"));
        assert!(!is_full_agent_tag("v0.49.3-rc1"));
        assert!(!is_full_agent_tag("0.49.3"));
        assert!(!is_full_agent_tag("v0.49"));
        assert!(!is_full_agent_tag("v0.49.3.1"));
        assert!(!is_full_agent_tag("vx.y.z"));
    }

    // ---- SHA256SUMS parse ----

    #[test]
    fn sha256_for_wheel_parses_both_marker_forms() {
        let body = "\
abc123  other-1.0.0-py3-none-any.whl
def456 *ados-0.49.4-py3-none-any.whl
";
        assert_eq!(
            sha256_for_wheel(body, "ados-0.49.4-py3-none-any.whl"),
            Some("def456".to_string())
        );
        assert_eq!(sha256_for_wheel(body, "missing.whl"), None);
    }

    // ---- checker logic against a fake source ----

    struct FakeSource {
        outcome: std::cell::RefCell<Option<FetchOutcome>>,
        sums: Option<String>,
    }

    impl FakeSource {
        fn ok(releases: Vec<Release>, sums: Option<String>) -> Self {
            FakeSource {
                outcome: std::cell::RefCell::new(Some(FetchOutcome::Ok {
                    releases,
                    etag: "etag-1".to_string(),
                })),
                sums,
            }
        }
        fn not_modified() -> Self {
            FakeSource {
                outcome: std::cell::RefCell::new(Some(FetchOutcome::NotModified)),
                sums: None,
            }
        }
    }

    impl ReleaseSource for FakeSource {
        fn fetch_releases(&self, _repo: &str, _etag: &str) -> FetchOutcome {
            self.outcome
                .borrow_mut()
                .take()
                .unwrap_or(FetchOutcome::NotModified)
        }
        fn fetch_sha256sums(&self, _url: &str) -> Option<String> {
            self.sums.clone()
        }
    }

    fn release(tag: &str, draft: bool, prerelease: bool, with_assets: bool) -> Release {
        let assets = if with_assets {
            vec![
                ReleaseAsset {
                    name: "ados-x-py3-none-any.whl".to_string(),
                    browser_download_url: "https://example/ados.whl".to_string(),
                    size: 123,
                },
                ReleaseAsset {
                    name: "SHA256SUMS".to_string(),
                    browser_download_url: "https://example/SHA256SUMS".to_string(),
                    size: 64,
                },
            ]
        } else {
            vec![]
        };
        Release {
            tag_name: tag.to_string(),
            draft,
            prerelease,
            assets,
            published_at: "2026-05-29T00:00:00Z".to_string(),
            body: "notes".to_string(),
            html_url: format!("https://example/releases/{tag}"),
        }
    }

    #[test]
    fn picks_newest_eligible_and_compares_against_current() {
        // Newest-first: a newer full-agent release than current is returned.
        let sums = "abc  ados-x-py3-none-any.whl\n".to_string();
        let src = FakeSource::ok(vec![release("v0.50.0", false, false, true)], Some(sums));
        let mut checker = UpdateChecker::new(UpdateConfig::default(), src);
        let m = checker.check_for_update("0.49.3").expect("update found");
        assert_eq!(m.version, "0.50.0");
        assert_eq!(m.sha256, "abc");
        assert_eq!(m.download_url, "https://example/ados.whl");
        assert_eq!(m.file_size, 123);
    }

    #[test]
    fn no_update_when_release_not_newer() {
        let src = FakeSource::ok(vec![release("v0.49.3", false, false, true)], None);
        let mut checker = UpdateChecker::new(UpdateConfig::default(), src);
        assert!(checker.check_for_update("0.49.3").is_none());
    }

    #[test]
    fn skips_draft_prerelease_and_non_full_agent_tags() {
        let releases = vec![
            release("lite-image-v0.99.0", false, false, true), // wrong tag line
            release("v0.99.1", true, false, true),             // draft
            release("v0.99.2", false, true, true),             // prerelease (stable)
            release("v0.60.0", false, false, true),            // the eligible one
        ];
        let sums = "abc  ados-x-py3-none-any.whl\n".to_string();
        let src = FakeSource::ok(releases, Some(sums));
        let mut checker = UpdateChecker::new(UpdateConfig::default(), src);
        let m = checker.check_for_update("0.49.3").expect("eligible found");
        assert_eq!(m.version, "0.60.0");
    }

    #[test]
    fn no_wheel_asset_yields_no_update() {
        let src = FakeSource::ok(vec![release("v0.60.0", false, false, false)], None);
        let mut checker = UpdateChecker::new(UpdateConfig::default(), src);
        assert!(checker.check_for_update("0.49.3").is_none());
    }

    #[test]
    fn cache_hit_returns_cached_only_when_newer() {
        // First poll resolves a manifest; a subsequent 304 returns it when it is
        // newer than current, and None when current has caught up.
        let sums = "abc  ados-x-py3-none-any.whl\n".to_string();
        let src1 = FakeSource::ok(vec![release("v0.60.0", false, false, true)], Some(sums));
        let mut checker = UpdateChecker::new(UpdateConfig::default(), src1);
        let _ = checker.check_for_update("0.49.3").expect("first poll");

        // Swap in a not-modified source for the next poll.
        checker.source = FakeSource::not_modified();
        // current still older than cached 0.60.0 → cached returned.
        assert_eq!(
            checker.check_for_update("0.49.3").map(|m| m.version),
            Some("0.60.0".to_string())
        );
        // current caught up → None on cache hit.
        assert!(checker.check_for_update("0.60.0").is_none());
    }
}
