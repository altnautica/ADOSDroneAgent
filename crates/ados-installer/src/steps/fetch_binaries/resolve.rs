//! Release resolution: which GitHub release the prebuilt binaries come from.

use super::*;

/// GitHub release-download base; each prebuilt asset hangs off
/// `<base>/<release tag>/<asset>` (plus `.sha256` / `.minisig` sidecars).
pub(super) const RELEASE_BASE: &str =
    "https://github.com/altnautica/ADOSDroneAgent/releases/download";

/// Environment override for [`RELEASE_BASE`], for tests only.
///
/// The whole fetch stack is anonymous `curl`, which speaks `file://`, so a fake release
/// tree on disk gives the pinned + unpinned resolution a full end-to-end exercise with
/// no network and no credential. It follows the driver layer's `ADOS_PREBUILT_BASE_URL`
/// (`scripts/drivers/lib-prebuilt.sh`), including the part that matters: an operator
/// never sets it, and nothing in the install writes it.
pub const RELEASE_BASE_ENV: &str = "ADOS_RELEASE_BASE";

/// The release-download base in force for this run.
pub(crate) fn release_base() -> String {
    base_or_default(std::env::var(RELEASE_BASE_ENV).ok().as_deref())
}

/// Pick the base from an override value (pure). A blank or whitespace-only
/// override is treated as absent: an exported-but-empty variable must not turn
/// every asset URL into a relative path curl would refuse.
pub(super) fn base_or_default(override_value: Option<&str>) -> String {
    match override_value.map(str::trim).filter(|v| !v.is_empty()) {
        Some(v) => v.to_string(),
        None => RELEASE_BASE.to_string(),
    }
}

/// The per-revision release tag CI publishes for one commit.
///
/// `rev-<full 40-char sha>`. The tag is derived in-shell inside the workflow
/// rather than declared as a job variable, because a templated tag is exactly
/// what the installer's release-tag guard refuses to accept.
pub fn rev_release_tag(rev: &str) -> String {
    format!("rev-{rev}")
}

/// Whether `rev` is the full 40-character object name CI names a release after.
///
/// The tag is `rev-<full sha>`, so a prefix cannot address it: `venv_agent`
/// expands one from the clone's own object store before this step runs. This is
/// the assertion of that contract rather than a fallback for it.
pub(super) fn is_full_object_name(rev: &str) -> bool {
    rev.len() == 40 && rev.chars().all(|c| c.is_ascii_hexdigit())
}

/// The failure text when the host arch has no published release binaries and
/// no `--artifacts` directory was given.
pub(super) fn no_release_assets(arch: &str) -> String {
    format!(
        "no prebuilt release binaries are published for {arch}; build the \
         workspace and pass --channel edge --artifacts <dir>"
    )
}

/// The failure text when a pin reaches the fetch still abbreviated.
///
/// Reachable in one narrow case: a RESUMED fresh install whose `venv`
/// checkpoint is already marked skips `venv_agent`, so nothing expanded the
/// prefix. Worth its own message, because the per-revision-release message
/// below would blame the workflow's path filter for what is really a skipped
/// step.
pub(super) fn rev_not_expanded(rev: &str) -> String {
    format!(
        "--ref {rev} is not a full 40-character commit, and the step that expands it \
         (the source checkout) did not run this time — a resumed install skips it once its \
         `venv` checkpoint is marked. Re-run with --force to redo the checkout, or pass the \
         full 40-character SHA so the per-revision release can be addressed directly."
    )
}

/// The version release a stable install resolves every asset from.
///
/// `v<X.Y.Z>`: the release workflow mirrors the tagged commit's service
/// binaries into the same release as the wheel and the deploy bundle, so the
/// stable channel places exactly the binaries that were built from the source it
/// installs. `version` is the bare form (`venv_agent::normalize_version`).
pub fn version_release_tag(version: &str) -> String {
    format!("v{version}")
}

/// The release a pinned install resolves every asset from (pure), or `None`
/// for an unpinned edge install, which reads the `prebuilt-main` manifest.
///
/// * `stable` is always pinned: to `v<version>`, and it refuses to run without
///   a `--version`. The edge pointer is whatever `main` last built, which is
///   exactly what a stable install must never place.
/// * Every other channel is pinned only by `--ref`, to `rev-<sha>`.
pub fn pinned_release(
    channel: &str,
    rev: Option<&str>,
    version: Option<&str>,
) -> Result<Option<String>, String> {
    if channel == "stable" {
        let version = version
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                "the stable channel installs the service binaries of one pinned release and \
             requires --version (the release to install); refusing to fall back to the \
             edge build"
                    .to_string()
            })?;
        return Ok(Some(version_release_tag(
            &crate::steps::venv_agent::normalize_version(version),
        )));
    }
    Ok(rev.map(rev_release_tag))
}

/// The release a run fetches from, resolved once before the install loop.
///
/// A pin (`rev-<sha>` for `--ref`, `v<X.Y.Z>` for stable) names the release
/// directly. An unpinned edge install reads the signed `prebuilt-main`
/// manifest and takes the single `rev-<sha>` release it names, so every
/// binary of one install comes from one commit.
pub(super) fn resolve_release(pin: Option<&str>, tmp_dir: &Path) -> anyhow::Result<ReleaseRef> {
    let base = release_base();
    match pin {
        Some(tag) => Ok(ReleaseRef::pinned(&base, tag)),
        None => {
            let m = manifest::fetch_manifest(&base, tmp_dir, SignaturePolicy::Required)?;
            Ok(ReleaseRef::from_manifest(&base, m))
        }
    }
}

/// The failure text when a pinned install finds no per-revision release.
///
/// It names the path filter, because that is the likely reason and it is not
/// guessable from a 404: `.github/workflows/rust.yml` only runs on changes under
/// `crates/**`, the four generated `.py` contract files, `data/systemd/**`, and
/// itself. A commit touching only `src/ados/**` therefore publishes nothing,
/// and the alternative — dropping the filter — pays ~23 arm64 release builds for
/// every commit to the Python tree.
pub fn rev_release_missing(rev: &str) -> String {
    format!(
        "no {tag} release exists, so the prebuilt binaries for revision {rev} were never \
         published. The workflow that builds them runs only on commits touching `crates/**`, \
         the generated `_*_generated.py` contract files, or `data/systemd/**` — a commit that \
         changed only `src/ados/**` publishes no per-revision release. Pin a commit that \
         touched the Rust tree, or re-run without --ref to install the build the \
         `prebuilt-main` manifest names.",
        tag = rev_release_tag(rev)
    )
}

/// The failure text when a stable install finds no service binaries in its
/// version release.
pub fn version_release_missing(tag: &str) -> String {
    format!(
        "the {tag} release carries no prebuilt service binaries, so the stable channel has \
         nothing pinned to install. Stable places only the binaries published with its own \
         release; it does not fall back to the edge build. Install a release \
         that carries its binaries, or use --channel edge."
    )
}

/// Whether the pinned release actually carries assets.
///
/// One sidecar fetch answers it: `<release>/<asset>.sha256` exists only if CI
/// published the binaries into that release. `sample` is a Hard-gated catalog
/// entry, so a present sidecar means the release holds the assets the install
/// cannot do without — not merely that a tag of that name exists. Cheap enough to
/// pay for on every pinned install (a few hundred bytes) and it converts an
/// opaque per-binary 404 into the one message that names the cause.
pub(super) fn pinned_release_published(
    release: &ReleaseRef,
    sample: &PrebuiltBinary,
    tmp_dir: &Path,
) -> bool {
    let url = format!("{}.sha256", release.asset_url(sample.asset));
    let probe = tmp_dir.join("pinned-release-probe.sha256");
    let ok = net::fetch(&url, &probe).is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}
