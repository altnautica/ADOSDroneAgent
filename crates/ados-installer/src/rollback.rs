//! Restore the previously-installed service binaries after a failed install.
//!
//! Every binary placement retains the outgoing copy as `<dest>.prev`
//! (see [`crate::steps::fetch_binaries::prev_sibling`]), and the fetch step
//! records each destination it replaced in this run. When a required step
//! fails after that, the installer calls [`roll_back`]: it swaps every replaced
//! binary back and restarts the units that run them, then exits non-zero. A
//! half-applied upgrade therefore leaves the node on the binaries it was
//! running before, not on a mix of new binaries and old units or config.
//!
//! Deliberately narrow. It restores **binaries only** — not the Python wheel,
//! not config, not systemd units. That bounds what it can promise: it recovers
//! the common bad-upgrade case, which is a Rust service that will not start or
//! a later step that could not finish, and it does not pretend to be a general
//! time machine. Only binaries replaced by THIS run are touched: a `.prev` left
//! by an earlier upgrade of a binary this run did not replace is not restored.

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

/// The systemd unit directories searched for the units that run a binary.
pub const UNIT_DIRS: &[&str] = &[
    "/etc/systemd/system",
    "/lib/systemd/system",
    "/usr/lib/systemd/system",
];

/// The units whose main process is one of `dests` (pure).
///
/// `units` is `(unit name, unit file body)`. A unit matches when an
/// `ExecStart=` line runs a destination path as its command (optionally behind
/// systemd's `-`/`+`/`!`/`@`/`:` prefixes). `ExecStartPre=` helpers are not
/// main processes and do not count, and a path that is only a prefix of the
/// command (`ados-display` vs `ados-display-probe`) does not match.
pub fn units_running(dests: &[PathBuf], units: &[(String, String)]) -> Vec<String> {
    let mut out: Vec<String> = units
        .iter()
        .filter(|(_, body)| {
            body.lines().any(|line| {
                let Some(cmd) = line.trim().strip_prefix("ExecStart=") else {
                    return false;
                };
                let cmd = cmd.trim_start_matches(['-', '+', '!', '@', ':']);
                let program = cmd.split_whitespace().next().unwrap_or("");
                dests.iter().any(|d| Path::new(program) == d.as_path())
            })
        })
        .map(|(name, _)| name.clone())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Every `*.service` file in `dirs`, as `(name, body)`. A name found in an
/// earlier directory shadows the same name in a later one, matching systemd's
/// own precedence (`/etc` over the vendor directories).
fn read_units(dirs: &[&str]) -> Vec<(String, String)> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".service") || !seen.insert(name.clone()) {
                continue;
            }
            if let Ok(body) = std::fs::read_to_string(entry.path()) {
                out.push((name, body));
            }
        }
    }
    out
}

/// What [`roll_back`] did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RollbackReport {
    /// Destinations whose retained copy is back in place.
    pub restored: Vec<PathBuf>,
    /// Destinations that had no retained copy and still hold the new binary.
    pub no_previous: Vec<PathBuf>,
    /// Destinations whose restore failed, with the reason.
    pub failed: Vec<(PathBuf, String)>,
    /// Units restarted onto the restored binaries.
    pub restarted: Vec<String>,
    /// Units whose restart failed.
    pub restart_failed: Vec<String>,
}

impl RollbackReport {
    /// One operator-facing line naming what was and was not restored.
    pub fn summary(&self) -> String {
        let names = |v: &[PathBuf]| {
            v.iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut parts = vec![format!("restored {}", self.restored.len())];
        if !self.no_previous.is_empty() {
            parts.push(format!("no previous copy: {}", names(&self.no_previous)));
        }
        if !self.failed.is_empty() {
            let failed: Vec<PathBuf> = self.failed.iter().map(|(p, _)| p.clone()).collect();
            parts.push(format!("restore failed: {}", names(&failed)));
        }
        if !self.restarted.is_empty() {
            parts.push(format!("restarted {}", self.restarted.join(", ")));
        }
        if !self.restart_failed.is_empty() {
            parts.push(format!(
                "restart failed: {}",
                self.restart_failed.join(", ")
            ));
        }
        parts.join("; ")
    }
}

/// Swap every binary in `replaced` back to its retained copy (pure apart from
/// the filesystem): no unit is touched.
pub fn restore_replaced(replaced: &[PathBuf]) -> RollbackReport {
    let mut unique: Vec<PathBuf> = replaced.to_vec();
    unique.sort();
    unique.dedup();
    let mut report = RollbackReport::default();
    for slot in plan_for(&unique) {
        match slot {
            SlotPlan::Restore { dest, prev } => match restore_one(&dest, &prev) {
                Ok(()) => report.restored.push(dest),
                Err(e) => report.failed.push((dest, e.to_string())),
            },
            SlotPlan::NoPrevious { dest } => report.no_previous.push(dest),
        }
    }
    report
}

/// Roll back a failed install: restore every binary this run replaced and
/// restart the enabled or active units that run them.
pub fn roll_back(replaced: &[PathBuf]) -> RollbackReport {
    let mut report = restore_replaced(replaced);
    for unit in units_running(&report.restored, &read_units(UNIT_DIRS)) {
        let wanted = crate::exec::run_ok("systemctl", &["is-enabled", "--quiet", &unit])
            || crate::exec::run_ok("systemctl", &["is-active", "--quiet", &unit]);
        if !wanted {
            continue;
        }
        if crate::exec::run_ok("systemctl", &["restart", "--no-block", &unit]) {
            report.restarted.push(unit);
        } else {
            report.restart_failed.push(unit);
        }
    }
    report
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

    fn tmp() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().to_path_buf();
        (dir, d)
    }

    #[test]
    fn a_binary_with_no_retained_copy_is_reported_not_skipped() {
        let (_dir, d) = tmp();
        let dest = d.join("ados-video");
        std::fs::write(&dest, b"current").unwrap();

        let plan = plan_for(std::slice::from_ref(&dest));
        assert_eq!(plan, vec![SlotPlan::NoPrevious { dest }]);
    }

    #[test]
    fn a_retained_copy_is_planned_for_restore() {
        let (_dir, d) = tmp();
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
        let (_dir, d) = tmp();
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
        let (_dir, d) = tmp();
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
    fn a_failed_run_restores_only_what_it_replaced() {
        let (_dir, d) = tmp();
        let video = d.join("ados-video");
        std::fs::write(&video, b"new video").unwrap();
        std::fs::write(prev_sibling(&video), b"old video").unwrap();
        let cloud = d.join("ados-cloud");
        std::fs::write(&cloud, b"first cloud").unwrap();
        // A binary this run did not replace keeps its stale `.prev` untouched.
        let radio = d.join("ados-radio");
        std::fs::write(&radio, b"current radio").unwrap();
        std::fs::write(prev_sibling(&radio), b"older radio").unwrap();

        let report = restore_replaced(&[video.clone(), cloud.clone(), video.clone()]);
        assert_eq!(report.restored, vec![video.clone()]);
        assert_eq!(report.no_previous, vec![cloud.clone()]);
        assert!(report.failed.is_empty());
        assert_eq!(std::fs::read(&video).unwrap(), b"old video");
        assert_eq!(std::fs::read(&cloud).unwrap(), b"first cloud");
        assert_eq!(std::fs::read(&radio).unwrap(), b"current radio");
    }

    #[test]
    fn only_units_whose_main_process_is_a_restored_binary_restart() {
        let dests = vec![
            PathBuf::from("/opt/ados/bin/ados-display"),
            PathBuf::from("/usr/local/bin/mediamtx"),
        ];
        let units = vec![
            (
                "ados-display.service".to_string(),
                "[Service]\nExecStart=/opt/ados/bin/ados-display --serve\n".to_string(),
            ),
            (
                "ados-display-probe.service".to_string(),
                "[Service]\nExecStart=/opt/ados/bin/ados-display-probe\n".to_string(),
            ),
            (
                "ados-mediamtx.service".to_string(),
                "[Service]\nExecStart=-/usr/local/bin/mediamtx /etc/ados/mediamtx.yml\n"
                    .to_string(),
            ),
            (
                "ados-plugin-x.service".to_string(),
                "[Service]\nExecStartPre=+/opt/ados/bin/ados-display x\nExecStart=/opt/x\n"
                    .to_string(),
            ),
        ];
        assert_eq!(
            units_running(&dests, &units),
            vec![
                "ados-display.service".to_string(),
                "ados-mediamtx.service".to_string()
            ]
        );
    }
}
