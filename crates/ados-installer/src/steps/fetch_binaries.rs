//! Fetch binaries: download + verify the prebuilt Rust service binaries for
//! the active profile and install them under `/opt/ados/bin`, then install the
//! global `ados*` symlinks. Required. Checkpoint `global-symlinks`.
//!
//! The load-bearing ordering invariant the whole crate exists to guarantee:
//! a Hard-gate binary (supervisor / video / cloud / vision) that cannot be
//! fetched-or-verified makes this step return [`StepOutcome::Failed`], so the
//! graph aborts BEFORE the systemd step runs. A best-effort binary that fails
//! is logged and skipped — the agent still comes up and reports the missing
//! capability.

use std::path::{Path, PathBuf};

use crate::binaries::{self, Gate, PrebuiltBinary};
use crate::ctx::Ctx;
use crate::env;
use crate::graph::{Step, StepKind, StepOutcome};
use crate::net;
use crate::verify::{self, Channel};

/// GitHub release-download base; each prebuilt asset hangs off
/// `<base>/<release_tag>/<asset>` (plus `.sha256` / `.minisig` sidecars).
const RELEASE_BASE: &str = "https://github.com/altnautica/ADOSDroneAgent/releases/download";

/// What to do with one binary's fetch-or-verify outcome, keyed off its catalog
/// gate. Pure: a Hard gate's failure aborts the install; a BestEffort gate's
/// failure degrades it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Proceed (success, or a best-effort miss we tolerate).
    Continue,
    /// A Hard-gated binary failed — the install must abort before systemd.
    FailRequired,
}

/// Map a service name + its fetch/verify success to a [`Decision`], keyed off
/// the catalog gate. Pure + unit-testable: no network, no catalog lookup beyond
/// the gate. A Hard gate failing → `FailRequired`; everything else `Continue`.
pub fn gate_outcome(gate: Gate, ok: bool) -> Decision {
    match (gate, ok) {
        (_, true) => Decision::Continue,
        (Gate::Hard, false) => Decision::FailRequired,
        (Gate::BestEffort, false) => Decision::Continue,
    }
}

/// Resolve the channel enum from the ctx's channel string. Anything that is not
/// exactly `stable` is treated as edge (the default + dev path).
fn channel_of(ctx: &Ctx) -> Channel {
    if ctx.channel == "stable" {
        Channel::Stable
    } else {
        Channel::Edge
    }
}

/// On edge we tolerate unsigned artifacts (CI may not yet sign); on stable a
/// signature is mandatory, so allow_unsigned is false there.
fn allow_unsigned_for(channel: Channel) -> bool {
    matches!(channel, Channel::Edge)
}

/// Fetch + verify one prebuilt binary into a temp dir, then install it 0755 at
/// its destination. Returns `Ok(())` on success, `Err` on any fetch/verify/
/// install miss (the caller maps that through the gate).
fn install_one(b: &PrebuiltBinary, tmp_dir: &Path, channel: Channel) -> anyhow::Result<()> {
    let asset_url = format!("{RELEASE_BASE}/{}/{}", b.release_tag, b.asset);
    let tmp_bin = tmp_dir.join(b.asset);
    let tmp_sha = tmp_dir.join(format!("{}.sha256", b.asset));
    let tmp_sig = tmp_dir.join(format!("{}.minisig", b.asset));

    // The binary + its sha256 are mandatory; the .minisig is best-effort so
    // verification upgrades to signature-checked automatically once CI signs.
    net::fetch(&asset_url, &tmp_bin)?;
    net::fetch(&format!("{asset_url}.sha256"), &tmp_sha)?;
    let _ = net::fetch(&format!("{asset_url}.minisig"), &tmp_sig);

    verify::verify_artifact(&tmp_bin, None, channel, allow_unsigned_for(channel))?;

    install_executable(&tmp_bin, Path::new(b.dest))?;
    Ok(())
}

/// Install a fetched file as a 0755 executable at `dest`, creating the parent
/// directory if needed. Uses the system `install -m 0755` when present (matches
/// the bash installer) and falls back to a copy + permission set otherwise.
fn install_executable(src: &Path, dest: &Path) -> anyhow::Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow::anyhow!("create {} failed: {e}", parent.display()))?;
    }
    let src_s = src.to_string_lossy();
    let dest_s = dest.to_string_lossy();
    let res = crate::exec::run("install", &["-m", "0755", &src_s, &dest_s]);
    if res.success() {
        return Ok(());
    }
    // Fallback: copy + chmod 0755 ourselves (e.g. `install` absent on the host).
    std::fs::copy(src, dest)
        .map_err(|e| anyhow::anyhow!("copy {} -> {} failed: {e}", src.display(), dest.display()))?;
    set_executable(dest)?;
    Ok(())
}

/// chmod 0755 (Unix); a no-op stub on non-Unix dev hosts.
#[cfg(unix)]
fn set_executable(dest: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o755);
    std::fs::set_permissions(dest, perms)
        .map_err(|e| anyhow::anyhow!("chmod 0755 {} failed: {e}", dest.display()))
}

#[cfg(not(unix))]
fn set_executable(_dest: &Path) -> anyhow::Result<()> {
    Ok(())
}

/// Install the global `/usr/local/bin/{ados,ados-agent}` symlinks pointing into
/// the venv's console scripts (the genuine "symlinks" part). Best-effort: a
/// symlink failure does not abort the install (the binaries are already on
/// disk), but it is logged.
fn install_global_symlinks() {
    let pairs = [
        (format!("{}/bin/ados", env::VENV_DIR), "/usr/local/bin/ados"),
        (
            format!("{}/bin/ados-agent", env::VENV_DIR),
            "/usr/local/bin/ados-agent",
        ),
    ];
    for (target, link) in pairs {
        // `ln -sf` overwrites an existing link idempotently.
        if !crate::exec::run_ok("ln", &["-sf", &target, link]) {
            tracing::warn!(target = %target, link, "global symlink install failed");
        }
    }
}

/// Prebuilt-binary fetch + global symlink install.
pub struct FetchBinaries;

impl Step for FetchBinaries {
    fn id(&self) -> &str {
        "fetch_binaries"
    }
    fn requires(&self) -> &[&str] {
        &["deps"]
    }
    fn checkpoint(&self) -> Option<&str> {
        Some("global-symlinks")
    }
    fn kind(&self) -> StepKind {
        StepKind::Required
    }
    fn run(&self, ctx: &mut Ctx) -> StepOutcome {
        // Prebuilt assets target aarch64 only. On a non-aarch64 dev host there
        // is nothing to fetch; skip cleanly (the bash path does the same).
        if !ctx.env.supported_arch {
            tracing::warn!(
                arch = %ctx.env.arch,
                "no prebuilt binaries for this arch; skipping fetch"
            );
            return StepOutcome::Skipped;
        }

        let channel = channel_of(ctx);
        let tmp_dir: PathBuf = match tempdir() {
            Ok(d) => d,
            Err(e) => return StepOutcome::Failed(format!("could not create temp dir: {e}")),
        };

        for b in binaries::for_profile(&ctx.profile) {
            let ok = match install_one(b, &tmp_dir, channel) {
                Ok(()) => {
                    tracing::info!(
                        service = b.service,
                        dest = b.dest,
                        "installed prebuilt binary"
                    );
                    true
                }
                Err(e) => {
                    tracing::warn!(service = b.service, error = %e, "prebuilt binary fetch/verify failed");
                    false
                }
            };
            // A Hard-gate miss aborts the install BEFORE systemd runs.
            if gate_outcome(b.gate, ok) == Decision::FailRequired {
                let _ = std::fs::remove_dir_all(&tmp_dir);
                return StepOutcome::Failed(format!(
                    "required prebuilt binary {} could not be installed",
                    b.service
                ));
            }
        }

        let _ = std::fs::remove_dir_all(&tmp_dir);

        // All Hard gates satisfied → install the global symlinks.
        install_global_symlinks();
        StepOutcome::Ok
    }
}

/// Create a unique temp directory under the system temp root for this run's
/// downloads. We roll our own (instead of pulling `tempfile` into the non-dev
/// build) using the pid + a monotonic counter.
fn tempdir() -> std::io::Result<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let base =
        std::env::temp_dir().join(format!("ados-installer-fetch-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&base)?;
    Ok(base)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binaries::PREBUILT;

    #[test]
    fn each_hard_gate_failing_means_fail_required() {
        for svc in ["ados-supervisor", "ados-video", "ados-cloud", "ados-vision"] {
            let b = PREBUILT.iter().find(|b| b.service == svc).unwrap();
            assert_eq!(b.gate, Gate::Hard, "{svc} must be a Hard gate");
            assert_eq!(
                gate_outcome(b.gate, false),
                Decision::FailRequired,
                "{svc} failing must abort the install"
            );
            // A Hard gate succeeding still continues.
            assert_eq!(gate_outcome(b.gate, true), Decision::Continue);
        }
    }

    #[test]
    fn best_effort_failing_continues() {
        // Pick a couple of best-effort catalog entries.
        for svc in ["ados-tui", "ados-mavlink-router", "ados-radio"] {
            let b = PREBUILT.iter().find(|b| b.service == svc).unwrap();
            assert_eq!(b.gate, Gate::BestEffort);
            assert_eq!(
                gate_outcome(b.gate, false),
                Decision::Continue,
                "{svc} (best-effort) failing must NOT abort the install"
            );
        }
    }

    #[test]
    fn channel_and_allow_unsigned_pairing() {
        // edge tolerates unsigned; stable does not.
        assert!(allow_unsigned_for(Channel::Edge));
        assert!(!allow_unsigned_for(Channel::Stable));
    }
}
