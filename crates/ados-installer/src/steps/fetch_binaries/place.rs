//! Placing binaries on disk, executable bits and global symlinks.

use super::*;

/// Install the onnx-enabled `ados-vision` binary together with the ONNX Runtime
/// shared library it dlopens at start. Both must land, AND the binary must
/// actually execve on this host — if the runtime library cannot be fetched, or
/// the placed binary is rejected by the dynamic loader (a glibc floor
/// mismatch), this returns `Err` and the caller falls back to the default
/// vision build.
pub(super) fn install_onnx_vision(
    tmp_dir: &Path,
    sink: &ProgressSink,
    source: &AssetSource<'_>,
) -> anyhow::Result<Vec<PathBuf>> {
    // Both halves of the variant come from the same release as the rest of
    // the install: this path is only reached for a release-sourced
    // `ados-vision` (see `install_service`).
    let source = *source;
    let mut replaced: Vec<PathBuf> =
        install_one_with_retry(&binaries::PREBUILT_VISION_ONNX, tmp_dir, sink, &source)?
            .into_iter()
            .collect();
    replaced.extend(
        install_one_with_retry(
            &binaries::PREBUILT_VISION_ONNX_RUNTIME,
            tmp_dir,
            sink,
            &source,
        )
        .map_err(|e| anyhow::anyhow!("ONNX Runtime library fetch failed: {e}"))?,
    );

    if !binary_execs_on_this_host(Path::new(binaries::PREBUILT_VISION_ONNX.dest)) {
        anyhow::bail!(
            "onnx vision binary rejected by the dynamic loader on this host \
             (glibc floor mismatch — the board's base image is older than the \
             onnx build's glibc floor)"
        );
    }
    Ok(replaced)
}

/// `<dest>.dl` sibling used as the verify-then-rename staging path. It lives in
/// the same directory as `dest` so the final `rename` is atomic.
pub(super) fn dl_sibling(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(".dl");
    PathBuf::from(s)
}

/// `<path>.<ext>` sidecar next to `path` (matches `verify_artifact`'s lookup).
pub(super) fn sidecar_path(path: &Path, ext: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".");
    s.push(ext);
    PathBuf::from(s)
}

/// The retained previous copy of a placed binary: `<dest>.prev`.
///
/// Kept so a bad upgrade has somewhere to go back to. Until this existed the
/// only documented recovery was reinstalling from a specific git ref, which
/// needs a working shell, internet and a version the operator knows — none of
/// which a customer necessarily has once the upgrade that broke the box has
/// already landed.
pub fn prev_sibling(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(".prev");
    PathBuf::from(s)
}

/// Atomically place the verified, already-chmod'd `src` at `dest`. A same-dir
/// `rename` swaps the inode in one step: it is never a half-written file, and a
/// running service that has the old binary mmap'd keeps its old inode (no
/// `ETXTBSY`, no `O_TRUNC` on a live executable). Falls back to a copy + chmod
/// only if the rename fails (e.g. a cross-filesystem dest the caller forced).
/// Returns whether an existing binary was replaced, so the run can record it
/// for [`crate::rollback::roll_back`].
pub(super) fn place_binary(src: &Path, dest: &Path) -> anyhow::Result<bool> {
    // Retain the outgoing binary before it is replaced. A hard link keeps the
    // old inode alive at a second name without a copy, so this costs no disk
    // and cannot half-finish; a rename would leave the destination briefly
    // absent, which is worse than the problem it solves. A failure here is not
    // fatal — losing the rollback copy is better than failing the install — but
    // it is logged, because a silent failure would present as a rollback that
    // is simply missing when it is needed most.
    let replaced = dest.exists();
    if replaced {
        let prev = prev_sibling(dest);
        let _ = std::fs::remove_file(&prev);
        if let Err(e) = std::fs::hard_link(dest, &prev) {
            if let Err(e2) = std::fs::copy(dest, &prev) {
                tracing::warn!(
                    link_error = %e,
                    copy_error = %e2,
                    dest = %dest.display(),
                    "could not retain the previous binary; rollback will not cover it"
                );
            }
        }
    }
    match std::fs::rename(src, dest) {
        Ok(()) => Ok(replaced),
        Err(_) => {
            // Non-atomic fallback for a dest on a different filesystem.
            std::fs::copy(src, dest).map_err(|e| {
                anyhow::anyhow!("copy {} -> {} failed: {e}", src.display(), dest.display())
            })?;
            set_executable(dest)?;
            let _ = std::fs::remove_file(src);
            Ok(replaced)
        }
    }
}

/// chmod 0755 (Unix); a no-op stub on non-Unix dev hosts.
#[cfg(unix)]
pub(super) fn set_executable(dest: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o755);
    std::fs::set_permissions(dest, perms)
        .map_err(|e| anyhow::anyhow!("chmod 0755 {} failed: {e}", dest.display()))
}

#[cfg(not(unix))]
pub(super) fn set_executable(_dest: &Path) -> anyhow::Result<()> {
    Ok(())
}

/// Global commands the install once linked whose target no longer exists. An
/// upgraded box would otherwise keep a dangling command on PATH, so each is
/// removed on every install, the same way `RETIRED_UNITS` prunes unit files.
pub(super) const RETIRED_GLOBAL_LINKS: &[&str] = &["/usr/local/bin/ados-agent"];

/// Install the global `/usr/local/bin/ados*` symlinks (the genuine "symlinks"
/// part). `ados` points into the venv's console script; `ados-supervisor`
/// points at the Rust binary under `/opt/ados/bin` so the operator command is
/// on PATH. This set mirrors the uninstall removal list so the two surfaces
/// never drift. Best-effort: a symlink failure does not abort the install (the
/// binaries are already on disk), but it is logged.
pub(super) fn install_global_symlinks() {
    for link in RETIRED_GLOBAL_LINKS {
        let _ = std::fs::remove_file(link);
    }
    for (target, link) in global_symlinks() {
        // `ln -sf` overwrites an existing link idempotently.
        if !crate::exec::run_ok("ln", &["-sf", &target, link]) {
            tracing::warn!(target = %target, link, "global symlink install failed");
        }
    }
}

/// The `(target, link)` pairs [`install_global_symlinks`] creates.
pub(crate) fn global_symlinks() -> [(String, &'static str); 2] {
    [
        (format!("{}/bin/ados", env::VENV_DIR), "/usr/local/bin/ados"),
        (
            format!("{}/ados-supervisor", env::BIN_DIR),
            "/usr/local/bin/ados-supervisor",
        ),
    ]
}
