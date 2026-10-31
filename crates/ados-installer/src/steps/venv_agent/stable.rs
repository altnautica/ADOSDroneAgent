//! The stable-channel install: signed release wheel or bundle, then a locked pip install.

use std::path::{Path, PathBuf};

use ados_term::exec;
use ados_term::progress::ProgressSink;
use anyhow::Context as _;

use crate::ctx::Ctx;
use crate::rollback::snapshots::ROLLBACK_DIR;
use crate::{net, verify};

use super::args::{
    bundle_filename, normalize_version, pip_install_lock_args, pip_install_wheel_args,
    release_asset_url, venv_pip, wheel_filename, BUNDLE_LOCK_MEMBER,
};
use super::edge::clone_dest;
use super::staging::{promote_staging, staging_dest};
use super::{on_pip_line, retain};

/// Install the agent package on the stable channel: download the release wheel
/// and the deploy bundle for the pinned `--version`, verify each against its
/// `.sha256` AND its `.minisig` (the signature is mandatory: a missing one, a
/// host without `minisign`, or a signature that does not match the embedded
/// trust anchor all refuse the install), install the dependencies from the
/// hashed lock inside the signed bundle (`--require-hashes --no-deps`), then
/// the wheel itself with `--no-deps`, then unpack the bundle into the persisted
/// source tree. Returns that tree so the caller records it into
/// `ctx.source_dir`: the systemd, udev and radio steps read `data/` and
/// `scripts/` from it, exactly as they read an edge clone.
///
/// Nothing is installed until BOTH artifacts have verified, so a release with a
/// bad bundle never leaves a new wheel beside an old tree, and a bundle without
/// the lock refuses the install rather than trusting the index. Downloads stage
/// in a fresh private directory (created exclusively, mode 0700), so nothing
/// another local user prepared can stand in for a verified file; it is removed
/// on every exit path.
///
/// A `--ref` pin never reaches here: `ctx::rev_channel_conflict` refuses the
/// stable+`--ref` pair before the install starts, because a `v<X.Y.Z>` release
/// tag addresses a version and not a commit, so there is no per-revision wheel
/// this could fetch.
pub(super) fn install_agent_stable(ctx: &Ctx) -> anyhow::Result<PathBuf> {
    let raw = ctx.args.version.as_deref().ok_or_else(|| {
        anyhow::anyhow!("stable channel requires --version (the release to install)")
    })?;
    let version = normalize_version(raw);

    // Dropping the TempDir removes the download tree, success or failure.
    let staging = tempfile::Builder::new()
        .prefix("ados-installer-wheel-")
        .tempdir()
        .context("create a private download directory")?;
    let dir = staging.path();
    let sink = ctx.progress.clone();
    let wheel_path = fetch_signed_release_asset(&version, &wheel_filename(&version), dir)?;
    let bundle_path = fetch_signed_release_asset(&version, &bundle_filename(&version), dir)?;
    sink.sub_log(
        "venv_agent",
        &format!("✓ v{version} wheel and deploy bundle signature-verified"),
    );

    let lock = extract_bundle_lock(&bundle_path, dir)?;
    let lock_s = lock.to_string_lossy().into_owned();
    run_venv_pip(
        &pip_install_lock_args(&lock_s),
        &sink,
        "pip install of the agent's hash-locked dependencies failed",
    )?;
    let wheel_s = wheel_path.to_string_lossy().into_owned();
    run_venv_pip(
        &pip_install_wheel_args(&wheel_s),
        &sink,
        "pip install of the agent wheel failed",
    )?;
    // Keep the verified wheel for the next upgrade's rollback.
    if let Err(e) = retain::keep_wheel(&wheel_path, Path::new(ROLLBACK_DIR)) {
        tracing::warn!(error = %e, "could not keep the agent wheel for rollback");
    }

    let repo = clone_dest()?;
    unpack_bundle(&bundle_path, &repo)?;
    Ok(repo)
}

/// Run the venv pip with `args`, streaming its output; `what` names a failure.
fn run_venv_pip(args: &[String], sink: &ProgressSink, what: &str) -> anyhow::Result<()> {
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let res = exec::run_streamed(&venv_pip(), &argv, on_pip_line(sink));
    if !res.spawned {
        anyhow::bail!("venv pip {} could not be spawned", venv_pip());
    }
    if !res.success() {
        anyhow::bail!("{what}: {}", res.stderr.trim());
    }
    Ok(())
}

/// Extract the hashed dependency lock from a verified deploy bundle into
/// `dir`. A bundle without it is refused: installing the wheel's dependencies
/// any other way would resolve them unpinned from the index.
pub(super) fn extract_bundle_lock(bundle: &Path, dir: &Path) -> anyhow::Result<PathBuf> {
    let bundle_s = bundle.to_string_lossy().into_owned();
    let dir_s = dir.to_string_lossy().into_owned();
    let res = exec::run(
        "tar",
        &["-xzf", &bundle_s, "-C", &dir_s, BUNDLE_LOCK_MEMBER],
    );
    let lock = dir.join(BUNDLE_LOCK_MEMBER);
    if !res.success() || !lock.is_file() {
        if !res.spawned {
            anyhow::bail!("tar is not installed; cannot read the deploy bundle");
        }
        anyhow::bail!(
            "the deploy bundle {} carries no hashed dependency lock ({BUNDLE_LOCK_MEMBER}); \
             refusing to resolve the agent's dependencies unpinned from the index",
            bundle.display()
        );
    }
    Ok(lock)
}

/// Fetch one asset of the `v<version>` release plus its `.sha256` and
/// `.minisig` into `dir`, and verify it on the stable posture: SHA256 and the
/// signature against [`verify::RELEASE_PUBKEY`] are both mandatory.
///
/// The `.minisig` fetch is allowed to fail on its own so the refusal comes from
/// the verifier, which names the artifact and the reason, rather than from a
/// bare 404.
fn fetch_signed_release_asset(version: &str, name: &str, dir: &Path) -> anyhow::Result<PathBuf> {
    let url = release_asset_url(version, name);
    let path = dir.join(name);
    net::fetch(&url, &path)?;
    net::fetch(&format!("{url}.sha256"), &sidecar(&path, "sha256"))?;
    let _ = net::fetch(&format!("{url}.minisig"), &sidecar(&path, "minisig"));
    verify_release_asset(&path)?;
    Ok(path)
}

/// The gate for a downloaded release asset: its `.sha256` AND a `.minisig`
/// from [`verify::RELEASE_PUBKEY`], both mandatory. A sha-only asset is
/// refused: the sidecar comes from the same host as the asset, so on its own it
/// proves integrity, not origin.
pub(super) fn verify_release_asset(path: &Path) -> anyhow::Result<()> {
    verify::verify_artifact(
        path,
        verify::RELEASE_PUBKEY,
        verify::SignaturePolicy::Required,
    )
}

/// Unpack a verified deploy bundle into the live source tree at `repo`.
///
/// The archive's single top-level `repo/` directory is extracted into a staging
/// sibling and promoted over the live tree only once extraction succeeded, the
/// same swap the edge clone uses, so a failed unpack leaves the previous tree
/// (and its `scripts/install.sh`) in place.
pub fn unpack_bundle(bundle: &Path, repo: &Path) -> anyhow::Result<()> {
    let staging = staging_dest(repo);
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)
        .with_context(|| format!("create the bundle staging dir {}", staging.display()))?;
    let bundle_s = bundle.to_string_lossy().into_owned();
    let staging_s = staging.to_string_lossy().into_owned();
    let res = exec::run("tar", &["-xzf", &bundle_s, "-C", &staging_s]);
    let unpacked = staging.join("repo");
    if !res.success() || !unpacked.is_dir() {
        let _ = std::fs::remove_dir_all(&staging);
        if !res.spawned {
            anyhow::bail!("tar is not installed; cannot unpack the deploy bundle");
        }
        anyhow::bail!(
            "could not unpack the deploy bundle {} (no top-level repo/ directory): {}",
            bundle.display(),
            res.stderr.trim()
        );
    }
    let promoted = promote_staging(&unpacked, repo);
    let _ = std::fs::remove_dir_all(&staging);
    promoted
}

/// `<path>.<ext>` sidecar next to `path` (matches `verify_sha256`'s lookup).
pub(super) fn sidecar(path: &Path, ext: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".");
    s.push(ext);
    PathBuf::from(s)
}
