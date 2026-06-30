//! Portable CPython provisioning.
//!
//! Some target boards ship a system Python older than the agent's 3.11 floor
//! (Debian 11 / bullseye ships 3.9, and bullseye has no `python3.11` apt
//! package — deadsnakes is Ubuntu-only). When [`crate::steps::deps::find_python`]
//! finds no acceptable system interpreter, the venv step calls [`provision`]:
//! it downloads a relocatable, self-contained CPython 3.11.x `install_only`
//! build — the same prebuilt CPython distribution `uv` uses, with no system
//! dependencies — selects the asset by architecture, verifies it against a
//! build-time-pinned SHA256, extracts it under the install root, and returns
//! the interpreter path the venv is then built on.
//!
//! The asset URL, the architecture → triple mapping, and the per-arch pinned
//! digest are pure so a unit test exercises them without the network; the
//! fetch, extract, and version-confirm orchestration in [`provision`] runs on
//! a real board.

use std::path::{Path, PathBuf};

use crate::env;
use crate::exec;
use crate::net;
use crate::verify;

/// GitHub release-download base for the portable CPython distribution.
const PBS_BASE: &str = "https://github.com/astral-sh/python-build-standalone/releases/download";

/// The pinned portable-CPython release tag (date-stamped, immutable).
const PBS_TAG: &str = "20260610";

/// The CPython version shipped in [`PBS_TAG`] (>= the agent's 3.11 floor).
const PBS_VERSION: &str = "3.11.15";

/// SHA256 of the `aarch64-unknown-linux-gnu` `install_only` asset for the pinned
/// tag + version. python-build-standalone publishes one aggregate `SHA256SUMS`
/// per release (not per-asset `.sha256` sidecars), so the digest is pinned in
/// this binary at build time — a stronger supply-chain pin than a sidecar
/// fetched from the same source as the tarball at runtime.
const SHA256_AARCH64: &str = "e8d907ca8b7c1b0686b6654d9a8e4fbb06a932351a62128aad16d0764d437120";

/// SHA256 of the `x86_64-unknown-linux-gnu` `install_only` asset for the pinned
/// tag + version (see [`SHA256_AARCH64`] for why the digest is pinned).
const SHA256_X86_64: &str = "33b167b995254ff6a3e1bff13deddc1220f3879e73e48a013b53bceaa89432ae";

/// The portable-CPython asset for `arch`: its target triple and the pinned
/// SHA256 of the `install_only` tarball, or `None` when no portable build is
/// published for that architecture. `arch` is the normalized [`env::arch`]
/// value (`arm64` already collapsed to `aarch64`). The triple and digest are
/// returned together so they can never drift apart. Pure.
pub fn asset_for_arch(arch: &str) -> Option<(&'static str, &'static str)> {
    match arch {
        "aarch64" => Some(("aarch64-unknown-linux-gnu", SHA256_AARCH64)),
        "x86_64" => Some(("x86_64-unknown-linux-gnu", SHA256_X86_64)),
        _ => None,
    }
}

/// Build the `install_only` asset filename (pure). The release names the asset
/// `cpython-<X.Y.Z>+<tag>-<triple>-install_only.tar.gz`.
pub fn asset_filename(version: &str, tag: &str, triple: &str) -> String {
    format!("cpython-{version}+{tag}-{triple}-install_only.tar.gz")
}

/// Build the asset download URL (pure): `<base>/<tag>/<filename>`.
pub fn asset_url(version: &str, tag: &str, triple: &str) -> String {
    format!("{PBS_BASE}/{tag}/{}", asset_filename(version, tag, triple))
}

/// The provisioned interpreter path (`<PORTABLE_PYTHON_DIR>/bin/python3`). The
/// `install_only` tarball extracts a single top-level `python/` directory, so
/// extracting it into the install root yields exactly this layout.
fn interpreter_path() -> String {
    format!("{}/bin/python3", env::PORTABLE_PYTHON_DIR)
}

/// Provision a portable CPython 3.11+ runtime and return its interpreter path.
///
/// Only called when no acceptable system interpreter exists (the venv step
/// keeps the system-python fast path). Idempotent: a previously-provisioned
/// runtime that still reports >= 3.11 is reused without a re-download.
pub fn provision() -> anyhow::Result<String> {
    let arch = env::arch();
    let (triple, sha256) = asset_for_arch(arch).ok_or_else(|| {
        anyhow::anyhow!(
            "no portable Python runtime is published for architecture `{arch}`; \
             install a Python 3.11+ interpreter on PATH and re-run the install"
        )
    })?;

    let interp = interpreter_path();

    // Reuse an already-provisioned runtime (idempotent re-runs, offline-safe).
    if Path::new(&interp).exists() && super::deps::python_is_311_plus(&interp) {
        tracing::info!(interpreter = %interp, "reusing the provisioned portable Python runtime");
        return Ok(interp);
    }

    let filename = asset_filename(PBS_VERSION, PBS_TAG, triple);
    let url = asset_url(PBS_VERSION, PBS_TAG, triple);
    tracing::warn!(version = PBS_VERSION, url = %url, "provisioning a portable Python runtime");

    // Stage the tarball under a unique temp dir; it is SHA256-verified against
    // the pinned digest before it ever touches the install root.
    let dir = staging_dir()?;
    let tarball = dir.join(&filename);
    let sha_path = sidecar(&tarball, "sha256");

    let outcome = (|| -> anyhow::Result<String> {
        net::fetch(&url, &tarball)?;

        // Write the pinned digest into a `.sha256` sidecar so the shared
        // in-process verifier (`sha2`) checks the download against it, exactly
        // as the release-wheel install does — only here the trusted digest comes
        // from this binary, not a runtime fetch.
        std::fs::write(&sha_path, format!("{sha256}  {filename}\n")).map_err(|e| {
            anyhow::anyhow!("could not stage the portable Python sha256 sidecar: {e}")
        })?;
        verify::verify_sha256(&tarball, &sha_path)?;

        // Replace any prior runtime, then extract the archive's `python/` dir to
        // the install root so the interpreter lands at PORTABLE_PYTHON_DIR.
        let _ = std::fs::remove_dir_all(env::PORTABLE_PYTHON_DIR);
        std::fs::create_dir_all(env::INSTALL_DIR).map_err(|e| {
            anyhow::anyhow!("cannot create the install root {}: {e}", env::INSTALL_DIR)
        })?;
        extract(&tarball, env::INSTALL_DIR)?;

        if !Path::new(&interp).exists() {
            anyhow::bail!(
                "portable Python extracted but {interp} is missing (unexpected archive layout)"
            );
        }
        // Confirm the extracted interpreter actually runs and reports >= 3.11
        // before the venv is built on it — extracting a tree is not proof it runs.
        if !super::deps::python_is_311_plus(&interp) {
            anyhow::bail!("provisioned interpreter {interp} did not report Python >= 3.11");
        }
        Ok(interp.clone())
    })();

    // Always remove the temp download tree, success or failure.
    let _ = std::fs::remove_dir_all(&dir);
    outcome
}

/// Extract a `.tar.gz` into `dest` via `tar` (a base-system tool present on
/// every Debian image). The pinned SHA256 already gates archive integrity, so
/// this is a plain extraction of a trusted tarball.
fn extract(tarball: &Path, dest: &str) -> anyhow::Result<()> {
    let tb = tarball.to_string_lossy();
    let res = exec::run("tar", &["-xzf", &tb, "-C", dest]);
    if res.success() {
        Ok(())
    } else if !res.spawned {
        anyhow::bail!("`tar` is not available to extract the portable Python runtime")
    } else {
        anyhow::bail!(
            "extracting the portable Python runtime failed: {}",
            res.stderr.trim()
        )
    }
}

/// `<path>.<ext>` sidecar next to `path` (matches `verify_sha256`'s lookup).
fn sidecar(path: &Path, ext: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".");
    s.push(ext);
    PathBuf::from(s)
}

/// A unique temp directory for the portable-python download (pid + a monotonic
/// counter), created under the system temp root.
fn staging_dir() -> std::io::Result<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let base =
        std::env::temp_dir().join(format!("ados-installer-python-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&base)?;
    Ok(base)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_for_known_arches_pairs_triple_and_digest() {
        let (triple, sha) = asset_for_arch("aarch64").expect("aarch64 is supported");
        assert_eq!(triple, "aarch64-unknown-linux-gnu");
        assert_eq!(sha, SHA256_AARCH64);

        let (triple, sha) = asset_for_arch("x86_64").expect("x86_64 is supported");
        assert_eq!(triple, "x86_64-unknown-linux-gnu");
        assert_eq!(sha, SHA256_X86_64);
    }

    #[test]
    fn asset_for_unsupported_arch_is_none() {
        // An unsupported arch refuses (no blind download), so provisioning fails
        // loudly instead of fetching a mismatched binary.
        assert!(asset_for_arch("riscv64").is_none());
        assert!(asset_for_arch("armv7").is_none());
    }

    #[test]
    fn pinned_digests_are_lowercase_64_hex() {
        for sha in [SHA256_AARCH64, SHA256_X86_64] {
            assert_eq!(sha.len(), 64, "a sha256 hex digest is 64 chars");
            assert!(
                sha.chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "the pinned digest must be lowercase hex: {sha}"
            );
        }
    }

    #[test]
    fn asset_filename_uses_the_install_only_naming() {
        assert_eq!(
            asset_filename("3.11.15", "20260610", "aarch64-unknown-linux-gnu"),
            "cpython-3.11.15+20260610-aarch64-unknown-linux-gnu-install_only.tar.gz"
        );
    }

    #[test]
    fn asset_url_hangs_the_filename_off_the_tag() {
        let url = asset_url("3.11.15", "20260610", "x86_64-unknown-linux-gnu");
        assert_eq!(
            url,
            "https://github.com/astral-sh/python-build-standalone/releases/download/20260610/cpython-3.11.15+20260610-x86_64-unknown-linux-gnu-install_only.tar.gz"
        );
        // The tag appears as a path segment and inside the filename.
        assert!(url.contains("/download/20260610/"));
        assert!(url.ends_with("-install_only.tar.gz"));
    }

    #[test]
    fn the_pinned_version_meets_the_floor() {
        // The pinned build must itself satisfy the >= 3.11 requirement the
        // provisioner enforces on the extracted interpreter.
        let mut parts = PBS_VERSION.split('.');
        let major: u32 = parts.next().unwrap().parse().unwrap();
        let minor: u32 = parts.next().unwrap().parse().unwrap();
        assert!(major > 3 || (major == 3 && minor >= 11));
    }

    #[test]
    fn interpreter_is_under_the_portable_dir() {
        assert_eq!(
            interpreter_path(),
            format!("{}/bin/python3", env::PORTABLE_PYTHON_DIR)
        );
    }
}
