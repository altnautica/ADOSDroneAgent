//! Which release an install reads its binaries from.
//!
//! CI builds every service binary in one job at one commit, publishes them to
//! the release `rev-<sha>`, then moves the single rolling release
//! `prebuilt-main` to a `manifest.json` naming that commit plus every asset's
//! SHA256 and size. An unpinned edge install reads the manifest first and
//! fetches every asset from the one release it names, so a node can never
//! hold binaries from two commits. The manifest is signed with the release
//! key, and its digests are the digests of record: an asset whose bytes do
//! not match its manifest entry is refused.
//!
//! `--ref <sha>` and the stable channel name their release directly and carry
//! no manifest; their assets verify against the per-asset `.sha256` sidecar
//! and signature.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::net;
use crate::verify::{self, SignaturePolicy};

/// The rolling release whose `manifest.json` names the current edge build.
pub const POINTER_TAG: &str = "prebuilt-main";

/// The manifest's asset name (on `prebuilt-main`, and in a bench
/// `--artifacts` directory).
pub const MANIFEST_ASSET: &str = "manifest.json";

/// Fetch attempts for the manifest, a fixed interval apart. The pointer's
/// three files are replaced one at a time, so a read that lands mid-move sees
/// a manifest and a signature from two different runs; the next attempt reads
/// a consistent set.
const MANIFEST_ATTEMPTS: u32 = 3;
const MANIFEST_RETRY: Duration = Duration::from_secs(3);

/// One asset's digest of record.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AssetDigest {
    /// Lowercase hex SHA256 of the asset bytes.
    pub sha256: String,
    /// Asset size in bytes.
    pub size: u64,
}

/// `manifest.json`: `{commit, agent_version, assets: {name: {sha256, size}}}`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Manifest {
    /// The full 40-character commit every asset was built from.
    pub commit: String,
    /// The agent package version that commit carries.
    pub agent_version: String,
    /// Every asset in the commit's `rev-<sha>` release.
    pub assets: BTreeMap<String, AssetDigest>,
}

impl Manifest {
    /// Parse and validate a manifest. A commit that is not a full object name
    /// cannot address a `rev-<sha>` release, and a digest that is not 64 hex
    /// characters cannot verify anything, so both are refused here rather
    /// than surfacing later as a 404 or a confusing digest mismatch.
    pub fn parse(bytes: &[u8]) -> anyhow::Result<Self> {
        let m: Manifest = serde_json::from_slice(bytes)
            .map_err(|e| anyhow::anyhow!("{MANIFEST_ASSET} is not a valid manifest: {e}"))?;
        if !super::is_full_object_name(&m.commit) {
            anyhow::bail!(
                "{MANIFEST_ASSET} names commit {:?}, not a full 40-character object name",
                m.commit
            );
        }
        if m.assets.is_empty() {
            anyhow::bail!("{MANIFEST_ASSET} for {} lists no assets", m.commit);
        }
        for (name, d) in &m.assets {
            if d.sha256.len() != 64 || !d.sha256.chars().all(|c| c.is_ascii_hexdigit()) {
                anyhow::bail!("{MANIFEST_ASSET} has a malformed sha256 for {name}");
            }
        }
        Ok(m)
    }

    /// The release every asset of this build hangs off.
    pub fn release_tag(&self) -> String {
        super::rev_release_tag(&self.commit)
    }

    /// The digest of record for `asset`. An asset the manifest does not list
    /// was not part of this build, so it is refused rather than fetched
    /// unverified.
    pub fn digest(&self, asset: &str) -> anyhow::Result<&AssetDigest> {
        self.assets.get(asset).ok_or_else(|| {
            anyhow::anyhow!(
                "{asset} is not listed in the {MANIFEST_ASSET} for commit {}",
                self.commit
            )
        })
    }

    /// Refuse bytes at `path` whose size differs from the manifest entry
    /// (the digest is checked by the verifier against the sidecar this
    /// entry writes).
    pub fn check_size(&self, asset: &str, path: &Path) -> anyhow::Result<&AssetDigest> {
        let want = self.digest(asset)?;
        let got = std::fs::metadata(path)
            .map_err(|e| anyhow::anyhow!("cannot stat {}: {e}", path.display()))?
            .len();
        if got != want.size {
            anyhow::bail!(
                "{asset} is {got} bytes but the manifest for {} says {}",
                self.commit,
                want.size
            );
        }
        Ok(want)
    }
}

/// Write a `sha256sum`-format sidecar carrying the manifest's digest, so the
/// one verifier checks the bytes against the signed manifest.
pub fn write_digest_sidecar(
    digest: &AssetDigest,
    asset: &str,
    sidecar: &Path,
) -> anyhow::Result<()> {
    std::fs::write(sidecar, format!("{}  {asset}\n", digest.sha256))
        .map_err(|e| anyhow::anyhow!("write {} failed: {e}", sidecar.display()))
}

/// The release one install reads every prebuilt asset from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseRef {
    base: String,
    tag: String,
    manifest: Option<Manifest>,
}

impl ReleaseRef {
    /// A release named directly (`rev-<sha>` for `--ref`, `v<X.Y.Z>` for
    /// stable); assets verify against their own sidecars.
    pub fn pinned(base: &str, tag: &str) -> Self {
        Self {
            base: base.to_string(),
            tag: tag.to_string(),
            manifest: None,
        }
    }

    /// The release a `prebuilt-main` manifest names; assets verify against
    /// the manifest's digests.
    pub fn from_manifest(base: &str, manifest: Manifest) -> Self {
        Self {
            base: base.to_string(),
            tag: manifest.release_tag(),
            manifest: Some(manifest),
        }
    }

    /// The release tag.
    pub fn tag(&self) -> &str {
        &self.tag
    }

    /// The manifest this release was resolved from, if any.
    pub fn manifest(&self) -> Option<&Manifest> {
        self.manifest.as_ref()
    }

    /// The download URL of one asset. The SOLE place a prebuilt asset URL is
    /// built, so a resolution cannot half-apply: every catalog binary, the
    /// onnx vision variant and the ONNX Runtime library come from one release.
    pub fn asset_url(&self, asset: &str) -> String {
        format!("{}/{}/{asset}", self.base, self.tag)
    }
}

/// The URL of the edge pointer's manifest under `base`.
pub fn manifest_url(base: &str) -> String {
    format!("{base}/{POINTER_TAG}/{MANIFEST_ASSET}")
}

/// Fetch, verify and parse the edge pointer's manifest under `base`, staging
/// it in `dir`. `policy` is [`SignaturePolicy::Required`] for every real
/// install: the manifest decides which bytes are trusted, so it carries the
/// release-key signature like every asset.
pub fn fetch_manifest(base: &str, dir: &Path, policy: SignaturePolicy) -> anyhow::Result<Manifest> {
    let url = manifest_url(base);
    let dest = dir.join(MANIFEST_ASSET);
    let mut attempt = 1;
    loop {
        let err = match fetch_manifest_once(&url, &dest, policy) {
            Ok(m) => return Ok(m),
            Err(e) => e,
        };
        if attempt >= MANIFEST_ATTEMPTS {
            anyhow::bail!("could not read the edge build manifest at {url}: {err}");
        }
        tracing::warn!(url = %url, attempt, error = %err, "edge manifest fetch failed; retrying");
        std::thread::sleep(MANIFEST_RETRY);
        attempt += 1;
    }
}

fn fetch_manifest_once(
    url: &str,
    dest: &Path,
    policy: SignaturePolicy,
) -> anyhow::Result<Manifest> {
    let sha = sidecar(dest, "sha256");
    let sig = sidecar(dest, "minisig");
    for stale in [dest, sha.as_path(), sig.as_path()] {
        let _ = std::fs::remove_file(stale);
    }
    net::fetch(url, dest)?;
    net::fetch(&format!("{url}.sha256"), &sha)?;
    // A missing signature is refused by the verifier, which names the reason.
    let _ = net::fetch(&format!("{url}.minisig"), &sig);
    verify::verify_artifact(dest, verify::RELEASE_PUBKEY, policy)?;
    let bytes =
        std::fs::read(dest).map_err(|e| anyhow::anyhow!("read {} failed: {e}", dest.display()))?;
    Manifest::parse(&bytes)
}

/// The manifest beside a bench `--artifacts` directory's binaries, if one is
/// there. Its keys are the staged file names.
pub fn local_manifest(dir: &Path) -> anyhow::Result<Option<Manifest>> {
    let path = dir.join(MANIFEST_ASSET);
    if !path.is_file() {
        return Ok(None);
    }
    let bytes =
        std::fs::read(&path).map_err(|e| anyhow::anyhow!("read {} failed: {e}", path.display()))?;
    Manifest::parse(&bytes).map(Some)
}

fn sidecar(path: &Path, ext: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".");
    s.push(ext);
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binaries::{self, PREBUILT};
    use crate::steps::fetch_binaries::tests::{fake_elf, sha256_sidecar_body};
    use crate::steps::fetch_binaries::{base_or_default, install_one_at, AssetSource};
    use ados_term::progress::ProgressSink;

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    fn digest_hex(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(bytes))
    }

    /// A manifest body naming `COMMIT` and `assets` (`name -> (sha256, size)`).
    fn manifest_body(assets: &BTreeMap<String, (String, u64)>) -> String {
        let assets: serde_json::Map<String, serde_json::Value> = assets
            .iter()
            .map(|(name, (sha, size))| {
                (
                    name.clone(),
                    serde_json::json!({"sha256": sha, "size": size}),
                )
            })
            .collect();
        serde_json::json!({"commit": COMMIT, "agent_version": "0.0.0", "assets": assets})
            .to_string()
    }

    /// Lay out the pointer the way CI publishes it, served over `file://`
    /// (curl speaks it, so the real fetch path runs with no network).
    fn publish_pointer(root: &Path, assets: &BTreeMap<String, (String, u64)>) -> String {
        let ptr = root.join(POINTER_TAG);
        std::fs::create_dir_all(&ptr).unwrap();
        let body = manifest_body(assets);
        std::fs::write(ptr.join(MANIFEST_ASSET), &body).unwrap();
        std::fs::write(
            ptr.join(format!("{MANIFEST_ASSET}.sha256")),
            sha256_sidecar_body(body.as_bytes(), MANIFEST_ASSET),
        )
        .unwrap();
        format!("file://{}", root.display())
    }

    #[test]
    fn edge_resolves_single_commit_manifest() {
        let root = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let fetched: Vec<&str> = PREBUILT
            .iter()
            .chain([
                &binaries::PREBUILT_VISION_ONNX,
                &binaries::PREBUILT_VISION_ONNX_RUNTIME,
            ])
            .map(|b| b.asset)
            .collect();
        let assets: BTreeMap<String, (String, u64)> = fetched
            .iter()
            .map(|a| (a.to_string(), (digest_hex(a.as_bytes()), a.len() as u64)))
            .collect();
        let base = publish_pointer(root.path(), &assets);

        // Unsigned in the fixture (the key is a CI secret); a real install
        // passes `Required`, which refuses the missing signature.
        let manifest = fetch_manifest(&base, scratch.path(), SignaturePolicy::LocalBuild)
            .expect("a well-formed pointer manifest resolves");
        assert_eq!(manifest.commit, COMMIT);
        let release = ReleaseRef::from_manifest(&base, manifest);
        assert_eq!(release.tag(), format!("rev-{COMMIT}"));
        // Every asset an install can fetch resolves under the ONE release the
        // manifest names, with the manifest's digest of record.
        for asset in fetched {
            assert_eq!(
                release.asset_url(asset),
                format!("{base}/rev-{COMMIT}/{asset}")
            );
            let d = release.manifest().unwrap().digest(asset).unwrap();
            assert_eq!(d.sha256, digest_hex(asset.as_bytes()));
        }
        assert!(release
            .manifest()
            .unwrap()
            .digest("ados-unknown-aarch64")
            .is_err());
    }

    #[test]
    fn edge_without_a_pointer_manifest_fails_naming_its_url() {
        let root = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let base = format!("file://{}", root.path().display());
        let err = fetch_manifest(&base, scratch.path(), SignaturePolicy::LocalBuild)
            .expect_err("no manifest, no edge install");
        assert!(err.to_string().contains(&manifest_url(&base)), "{err}");
    }

    #[test]
    fn asset_sha_mismatch_refused() {
        let root = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let video = PREBUILT.iter().find(|b| b.service == "ados-video").unwrap();

        // The release holds aarch64 bytes WITH a matching per-asset sidecar, so
        // only the manifest can be what refuses them.
        let bytes = fake_elf(0xB7, b"the published binary");
        let rev = root.path().join(format!("rev-{COMMIT}"));
        std::fs::create_dir_all(&rev).unwrap();
        std::fs::write(rev.join(video.asset), &bytes).unwrap();
        std::fs::write(
            rev.join(format!("{}.sha256", video.asset)),
            sha256_sidecar_body(&bytes, video.asset),
        )
        .unwrap();
        let mut assets = BTreeMap::new();
        assets.insert(
            video.asset.to_string(),
            ("0".repeat(64), bytes.len() as u64),
        );
        let base = format!("file://{}", root.path().display());
        let manifest = Manifest::parse(manifest_body(&assets).as_bytes()).unwrap();
        let release = ReleaseRef::from_manifest(&base, manifest);

        let dest = dst.path().join("ados-video");
        std::fs::write(&dest, b"the binary already installed").unwrap();
        let source = AssetSource::Release {
            release: Some(&release),
            host_arch: "aarch64",
        };
        let err = install_one_at(
            video,
            &dest,
            scratch.path(),
            &ProgressSink::default(),
            &source,
        )
        .expect_err("bytes that differ from the manifest digest must be refused");
        assert!(
            err.to_string().contains("SHA256 verification failed"),
            "{err}"
        );
        assert_eq!(
            std::fs::read(&dest).unwrap(),
            b"the binary already installed",
            "a refused asset must leave the running binary alone"
        );
    }

    #[test]
    fn a_manifest_must_name_a_full_commit_and_real_digests() {
        let mut assets = BTreeMap::new();
        assets.insert("ados-video-aarch64".to_string(), ("ab".repeat(32), 1));
        assert!(Manifest::parse(manifest_body(&assets).as_bytes()).is_ok());
        let short = manifest_body(&assets).replace(COMMIT, &COMMIT[..8]);
        assert!(Manifest::parse(short.as_bytes()).is_err());
        assets.insert("ados-tui-aarch64".to_string(), ("zz".repeat(32), 1));
        assert!(Manifest::parse(manifest_body(&assets).as_bytes()).is_err());
        assert!(Manifest::parse(b"{}").is_err());
    }

    #[test]
    fn a_pinned_release_hangs_every_asset_off_its_own_tag() {
        // The exact URL, not its shape: this string is what a board resolves.
        let base = base_or_default(None);
        let rev = ReleaseRef::pinned(&base, &format!("rev-{COMMIT}"));
        assert_eq!(
            rev.asset_url("ados-supervisor-aarch64"),
            format!(
                "https://github.com/altnautica/ADOSDroneAgent/releases/download/rev-{COMMIT}/ados-supervisor-aarch64"
            )
        );
        let stable = ReleaseRef::pinned(&base, "v0.99.376");
        assert_eq!(
            stable.asset_url("ados-video-aarch64"),
            "https://github.com/altnautica/ADOSDroneAgent/releases/download/v0.99.376/ados-video-aarch64"
        );
        assert!(rev.manifest().is_none());
        assert_eq!(
            manifest_url(&base),
            "https://github.com/altnautica/ADOSDroneAgent/releases/download/prebuilt-main/manifest.json"
        );
    }
}
