//! Point chrony at the GNSS PPS refclock `ados-clock` feeds.
//!
//! When a GNSS PPS source is locked, `ados-clock` sends one `sock_sample`
//! datagram per pulse to `/run/chrony.ados.sock`, the socket chrony binds for a
//! `refclock SOCK` line. This step writes that line as the drop-in
//! `/etc/chrony/conf.d/ados.conf` on the profiles that run the clock, when
//! chrony is installed (`/etc/chrony` exists). Idempotent: the file is only
//! rewritten, and chrony only restarted, when the content differs. With no
//! chrony on the box the step is a no-op.

use std::io;
use std::path::Path;

use crate::ctx::Ctx;
use crate::graph::{Step, StepKind, StepOutcome};
use ados_term::exec;

/// The drop-in, relative to the filesystem root.
pub const CONF_REL: &str = "etc/chrony/conf.d/ados.conf";
/// The chrony config directory whose presence means chrony is installed.
const CHRONY_DIR_REL: &str = "etc/chrony";

/// The drop-in body.
pub const CONF_BODY: &str = "\
# Written by ADOS: GNSS PPS samples from ados-clock, sent only while its PPS
# source is locked.
refclock SOCK /run/chrony.ados.sock refid PPS precision 1e-7 poll 4
";

/// What [`ensure_conf`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfOutcome {
    /// chrony is not installed under `root`.
    NoChrony,
    /// The drop-in already had this content.
    Unchanged,
    /// The drop-in was created or rewritten.
    Written,
}

/// Ensure the drop-in under `root` (`/` on a real install).
pub fn ensure_conf(root: &Path) -> io::Result<ConfOutcome> {
    if !root.join(CHRONY_DIR_REL).is_dir() {
        return Ok(ConfOutcome::NoChrony);
    }
    let path = root.join(CONF_REL);
    if std::fs::read_to_string(&path).is_ok_and(|cur| cur == CONF_BODY) {
        return Ok(ConfOutcome::Unchanged);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, CONF_BODY)?;
    Ok(ConfOutcome::Written)
}

/// Remove the drop-in under `root` (`/` on a real install), the uninstall's
/// reverse of [`ensure_conf`]. Returns whether a file was removed; a missing
/// file is already removed.
pub fn remove_conf(root: &Path) -> io::Result<bool> {
    match std::fs::remove_file(root.join(CONF_REL)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Uninstall: remove the drop-in and, when one was there, restart chrony so
/// it drops the refclock that pointed at the gone `ados-clock`.
pub fn uninstall() {
    match remove_conf(Path::new("/")) {
        Ok(true) => {
            let r = exec::run("systemctl", &["try-restart", "chrony.service"]);
            if !r.success() {
                tracing::warn!(stderr = %r.stderr.trim(), "chrony restart failed");
            }
        }
        Ok(false) => {}
        Err(e) => tracing::warn!(error = %e, path = CONF_REL, "chrony drop-in removal failed"),
    }
}

/// Whether `profile` runs `ados-clock`.
fn profile_runs_clock(profile: &str) -> bool {
    matches!(profile, "drone" | "ground_station" | "visor")
}

/// Write the chrony refclock drop-in for the GNSS PPS feed.
pub struct ChronyAdos;

impl Step for ChronyAdos {
    fn id(&self) -> &str {
        "chrony_ados"
    }
    fn requires(&self) -> &[&str] {
        // After the packages, so an apt-installed chrony is already present.
        &["deps"]
    }
    fn checkpoint(&self) -> Option<&str> {
        // Re-affirm on every upgrade (idempotent).
        None
    }
    fn kind(&self) -> StepKind {
        // Time discipline is an improvement, never a reason to fail an install.
        StepKind::Optional
    }
    fn run(&self, ctx: &mut Ctx) -> StepOutcome {
        if !profile_runs_clock(&ctx.profile) {
            return StepOutcome::Skipped;
        }
        match ensure_conf(Path::new("/")) {
            Ok(ConfOutcome::NoChrony) => StepOutcome::Skipped,
            Ok(ConfOutcome::Unchanged) => StepOutcome::Ok,
            Ok(ConfOutcome::Written) => {
                // chronyd reads refclock lines only at start, so the new line
                // takes a restart; try-restart leaves a stopped chrony stopped.
                let r = exec::run("systemctl", &["try-restart", "chrony.service"]);
                if !r.success() {
                    tracing::warn!(stderr = %r.stderr.trim(), "chrony restart failed");
                }
                StepOutcome::Ok
            }
            Err(e) => {
                tracing::warn!(error = %e, path = CONF_REL, "chrony drop-in write failed");
                StepOutcome::Ok
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_once_then_is_unchanged() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("etc/chrony")).unwrap();
        assert_eq!(ensure_conf(root.path()).unwrap(), ConfOutcome::Written);
        let body = std::fs::read_to_string(root.path().join(CONF_REL)).unwrap();
        assert_eq!(body, CONF_BODY);
        assert!(body
            .lines()
            .any(|l| l == "refclock SOCK /run/chrony.ados.sock refid PPS precision 1e-7 poll 4"));
        assert_eq!(ensure_conf(root.path()).unwrap(), ConfOutcome::Unchanged);
    }

    #[test]
    fn a_drifted_file_is_rewritten() {
        let root = tempfile::tempdir().unwrap();
        let conf = root.path().join(CONF_REL);
        std::fs::create_dir_all(conf.parent().unwrap()).unwrap();
        std::fs::write(&conf, "refclock SOCK /tmp/other.sock refid GPS\n").unwrap();
        assert_eq!(ensure_conf(root.path()).unwrap(), ConfOutcome::Written);
        assert_eq!(std::fs::read_to_string(&conf).unwrap(), CONF_BODY);
    }

    #[test]
    fn no_chrony_is_a_no_op() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(ensure_conf(root.path()).unwrap(), ConfOutcome::NoChrony);
        assert!(!root.path().join("etc").exists());
    }

    #[test]
    fn removal_reverses_the_drop_in_and_is_idempotent() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("etc/chrony")).unwrap();
        ensure_conf(root.path()).unwrap();
        assert!(remove_conf(root.path()).unwrap());
        assert!(!root.path().join(CONF_REL).exists());
        // chrony's own config directory stays.
        assert!(root.path().join("etc/chrony").is_dir());
        assert!(!remove_conf(root.path()).unwrap());
    }

    #[test]
    fn runs_on_the_clock_profiles_only() {
        for p in ["drone", "ground_station", "visor"] {
            assert!(profile_runs_clock(p), "{p}");
        }
        for p in ["workstation", "compute"] {
            assert!(!profile_runs_clock(p), "{p}");
        }
    }
}
