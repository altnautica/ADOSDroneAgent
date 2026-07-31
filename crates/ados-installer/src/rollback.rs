//! Restore the previously-installed service binaries.
//!
//! Every binary placement retains the outgoing copy as `<dest>.prev`
//! (see [`crate::steps::fetch_binaries::prev_sibling`]). This module swaps those
//! back and restarts the affected units, so a bad upgrade has a recovery path
//! that does not require internet, a git ref, or knowing which version was good.
//!
//! Deliberately narrow. It restores **binaries only** — not the Python wheel,
//! not config, not systemd units. That bounds what it can promise: it recovers
//! the common bad-upgrade case, which is a Rust service that will not start or
//! misbehaves, and it does not pretend to be a general time machine. A rollback
//! that silently half-reverted would be worse than none, so the scope is stated
//! rather than implied, and reported back to the operator on every run.

use std::path::{Path, PathBuf};

use crate::steps::fetch_binaries::prev_sibling;

/// What a rollback would do to one binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotPlan {
    /// A retained copy exists and would be restored.
    Restore { dest: PathBuf, prev: PathBuf },
    /// No retained copy: this binary has only ever been installed once, or its
    /// retention failed. Reported rather than skipped silently, because "some
    /// of your services rolled back" is something the operator must know.
    NoPrevious { dest: PathBuf },
}

/// Decide, per binary, what a rollback can do. Pure apart from the existence
/// checks, so the reporting is testable without touching a real install.
pub fn plan_for(dests: &[PathBuf]) -> Vec<SlotPlan> {
    dests
        .iter()
        .map(|dest| {
            let prev = prev_sibling(dest);
            if prev.exists() {
                SlotPlan::Restore {
                    dest: dest.clone(),
                    prev,
                }
            } else {
                SlotPlan::NoPrevious { dest: dest.clone() }
            }
        })
        .collect()
}

/// The binaries a rollback covers for `profile`.
pub fn dests_for_profile(profile: &str) -> Vec<PathBuf> {
    crate::binaries::for_profile(profile)
        .into_iter()
        .map(|b| PathBuf::from(b.dest))
        .collect()
}

/// Swap one retained copy back into place.
///
/// The current binary is moved to a scratch name first rather than deleted, so
/// a failure part-way leaves something executable at `dest` instead of a hole.
/// The scratch copy then becomes the new `.prev`, which makes the operation its
/// own inverse: rolling back twice returns to where you started, rather than
/// stranding the operator one version deep with no way forward.
pub fn restore_one(dest: &Path, prev: &Path) -> std::io::Result<()> {
    let scratch = {
        let mut s = dest.as_os_str().to_owned();
        s.push(".rollback-scratch");
        PathBuf::from(s)
    };
    let _ = std::fs::remove_file(&scratch);
    if dest.exists() {
        std::fs::rename(dest, &scratch)?;
    }
    if let Err(e) = std::fs::rename(prev, dest) {
        // Put the current binary back; a failed rollback must not leave the
        // destination empty.
        if scratch.exists() {
            let _ = std::fs::rename(&scratch, dest);
        }
        return Err(e);
    }
    if scratch.exists() {
        let _ = std::fs::rename(&scratch, prev_sibling(dest));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "ados-rollback-{}-{}-{name}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_binary_with_no_retained_copy_is_reported_not_skipped() {
        let d = tmp("noprev");
        let dest = d.join("ados-video");
        std::fs::write(&dest, b"current").unwrap();

        let plan = plan_for(std::slice::from_ref(&dest));
        assert_eq!(plan, vec![SlotPlan::NoPrevious { dest }]);
    }

    #[test]
    fn a_retained_copy_is_planned_for_restore() {
        let d = tmp("hasprev");
        let dest = d.join("ados-video");
        std::fs::write(&dest, b"new").unwrap();
        std::fs::write(prev_sibling(&dest), b"old").unwrap();

        match &plan_for(std::slice::from_ref(&dest))[0] {
            SlotPlan::Restore { dest: p, .. } => assert_eq!(p, &dest),
            other => panic!("expected a restore, got {other:?}"),
        }
    }

    #[test]
    fn restore_swaps_and_is_its_own_inverse() {
        let d = tmp("swap");
        let dest = d.join("ados-video");
        std::fs::write(&dest, b"new").unwrap();
        std::fs::write(prev_sibling(&dest), b"old").unwrap();

        restore_one(&dest, &prev_sibling(&dest)).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"old");
        assert_eq!(
            std::fs::read(prev_sibling(&dest)).unwrap(),
            b"new",
            "the replaced binary becomes the new retained copy"
        );

        // Rolling back again returns to where we started, so an operator who
        // rolls back by mistake is not stranded one version deep.
        restore_one(&dest, &prev_sibling(&dest)).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"new");
        assert_eq!(std::fs::read(prev_sibling(&dest)).unwrap(), b"old");
    }

    #[test]
    fn a_failed_restore_leaves_something_executable_at_dest() {
        let d = tmp("failed");
        let dest = d.join("ados-video");
        std::fs::write(&dest, b"current").unwrap();
        // A retained path that does not exist: the rename will fail.
        let missing = d.join("ados-video.absent");

        assert!(restore_one(&dest, &missing).is_err());
        assert_eq!(
            std::fs::read(&dest).unwrap(),
            b"current",
            "a failed rollback must not leave the destination empty"
        );
    }

    #[test]
    fn the_plan_covers_the_profiles_own_binaries() {
        let drone = dests_for_profile("drone");
        assert!(!drone.is_empty(), "a drone has binaries to roll back");
        // Absolute paths only — a relative dest would resolve against whatever
        // directory the installer happened to run from.
        assert!(drone.iter().all(|p| p.is_absolute()));
        // Not every target is under the ADOS prefix: the vendored media server
        // lands in /usr/local/bin. Rollback follows the catalog rather than
        // assuming a prefix, so a binary placed outside it is still covered.
        assert!(
            drone.iter().any(|p| !p.starts_with("/opt/ados")),
            "the catalog places at least one binary outside the ADOS prefix"
        );
    }
}
