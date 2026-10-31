//! The agent wheel and config snapshots one install run takes, and their
//! restore.
//!
//! `venv_agent` keeps every installed agent wheel under [`ROLLBACK_DIR`]; before
//! it installs a new version it records the retained wheel of the version it is
//! replacing. `config_migrate` copies `/etc/ados/config.yaml` to
//! `config.yaml.pre-<ver>` before migrating and records that copy. A failed run
//! restores both alongside the binaries.
//!
//! The record is scoped to one process: its first line is `run <pid>`, and a
//! record written by another run (an earlier, successful one) is ignored and
//! replaced. So a run that fails before taking any snapshot never restores a
//! stale one.

use std::path::{Path, PathBuf};

/// Where the retained agent wheels live.
pub const ROLLBACK_DIR: &str = "/opt/ados/rollback";

/// The record of the snapshots this run took.
pub const RUN_RECORD: &str = "/opt/ados/rollback/this-run";

/// Record kind for the previous agent wheel.
pub const KIND_WHEEL: &str = "wheel";

/// Record kind for the pre-migration config copy.
pub const KIND_CONFIG: &str = "config";

/// The snapshots this run recorded.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RunSnapshots {
    /// The retained wheel of the version this run replaced.
    pub wheel: Option<PathBuf>,
    /// The config as it was before this run migrated it.
    pub config: Option<PathBuf>,
}

fn run_header() -> String {
    format!("run {}", std::process::id())
}

/// Start this run's record empty, discarding whatever an earlier run left.
pub fn begin_run_record(record: &Path) -> std::io::Result<()> {
    if let Some(parent) = record.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(record, run_header() + "\n")
}

/// Record that this run took a `kind` snapshot at `path`, replacing any earlier
/// entry of the same kind, and discarding a record left by another run.
pub fn record_snapshot(record: &Path, kind: &str, path: &Path) -> std::io::Result<()> {
    let header = run_header();
    let existing = std::fs::read_to_string(record).unwrap_or_default();
    let mut lines: Vec<String> = vec![header.clone()];
    if existing.lines().next() == Some(header.as_str()) {
        let prefix = format!("{kind} ");
        lines.extend(
            existing
                .lines()
                .skip(1)
                .filter(|l| !l.starts_with(&prefix))
                .map(str::to_string),
        );
    }
    lines.push(format!("{kind} {}", path.display()));
    if let Some(parent) = record.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(record, lines.join("\n") + "\n")
}

/// The snapshots this run recorded. Empty for an absent record or one written
/// by another run.
pub fn read_run_snapshots(record: &Path) -> RunSnapshots {
    let Ok(body) = std::fs::read_to_string(record) else {
        return RunSnapshots::default();
    };
    let mut lines = body.lines();
    if lines.next() != Some(run_header().as_str()) {
        return RunSnapshots::default();
    }
    let mut snaps = RunSnapshots::default();
    for line in lines {
        match line.split_once(' ') {
            Some((KIND_WHEEL, p)) => snaps.wheel = Some(PathBuf::from(p)),
            Some((KIND_CONFIG, p)) => snaps.config = Some(PathBuf::from(p)),
            _ => {}
        }
    }
    snaps
}

/// What restoring the snapshots did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SnapshotRestore {
    /// The wheel reinstalled, if any.
    pub wheel: Option<PathBuf>,
    /// The config copy put back, if any.
    pub config: Option<PathBuf>,
    /// Snapshots whose restore failed, with the reason.
    pub failed: Vec<(PathBuf, String)>,
}

/// Put the recorded snapshots back: the config copy over `config_target`
/// (atomically, owner-only, since it carries secrets) and the wheel through
/// `install_wheel`.
pub fn restore_snapshots(
    snaps: &RunSnapshots,
    config_target: &Path,
    install_wheel: impl FnOnce(&Path) -> Result<(), String>,
) -> SnapshotRestore {
    let mut out = SnapshotRestore::default();
    if let Some(copy) = &snaps.config {
        let restored = std::fs::read(copy)
            .and_then(|bytes| crate::env::write_atomic_durable(config_target, &bytes, Some(0o600)));
        match restored {
            Ok(()) => out.config = Some(copy.clone()),
            Err(e) => out.failed.push((copy.clone(), e.to_string())),
        }
    }
    if let Some(wheel) = &snaps.wheel {
        match install_wheel(wheel) {
            Ok(()) => out.wheel = Some(wheel.clone()),
            Err(e) => out.failed.push((wheel.clone(), e)),
        }
    }
    out
}

/// Reinstall a retained wheel into the agent venv, exactly as it was.
pub fn reinstall_wheel(wheel: &Path) -> Result<(), String> {
    let wheel_s = wheel.to_string_lossy().into_owned();
    let res = ados_term::exec::run(
        &crate::steps::venv_agent::venv_pip(),
        &["install", "--no-deps", "--force-reinstall", &wheel_s],
    );
    if res.success() {
        Ok(())
    } else {
        Err(format!("pip reinstall failed: {}", res.stderr.trim()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollback_restores_wheel_and_config() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("rollback/this-run");
        let config = dir.path().join("config.yaml");
        let pre = dir.path().join("config.yaml.pre-0.103.0");
        let wheel = dir.path().join("ados_drone_agent-0.102.0-py3-none-any.whl");
        std::fs::write(&pre, "agent:\n  profile: drone\n").unwrap();
        std::fs::write(&config, "agent:\n  profile: drone\nmigrated: true\n").unwrap();
        std::fs::write(&wheel, b"wheel").unwrap();

        record_snapshot(&record, KIND_WHEEL, &wheel).unwrap();
        record_snapshot(&record, KIND_CONFIG, &pre).unwrap();
        let snaps = read_run_snapshots(&record);
        assert_eq!(snaps.wheel.as_deref(), Some(wheel.as_path()));
        assert_eq!(snaps.config.as_deref(), Some(pre.as_path()));

        let mut installed = None;
        let out = restore_snapshots(&snaps, &config, |w| {
            installed = Some(w.to_path_buf());
            Ok(())
        });
        assert_eq!(installed.as_deref(), Some(wheel.as_path()));
        assert_eq!(out.wheel.as_deref(), Some(wheel.as_path()));
        assert_eq!(out.config.as_deref(), Some(pre.as_path()));
        assert!(out.failed.is_empty());
        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            "agent:\n  profile: drone\n"
        );
    }

    #[test]
    fn a_record_from_another_run_restores_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("this-run");
        std::fs::write(&record, "run 0\nconfig /tmp/stale\nwheel /tmp/stale.whl\n").unwrap();
        assert_eq!(read_run_snapshots(&record), RunSnapshots::default());

        // Recording into it starts this run's record afresh.
        let pre = dir.path().join("config.yaml.pre-1.0.0");
        record_snapshot(&record, KIND_CONFIG, &pre).unwrap();
        let snaps = read_run_snapshots(&record);
        assert_eq!(snaps.config.as_deref(), Some(pre.as_path()));
        assert_eq!(snaps.wheel, None);
    }

    #[test]
    fn a_failed_wheel_reinstall_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let snaps = RunSnapshots {
            wheel: Some(dir.path().join("a.whl")),
            config: None,
        };
        let out = restore_snapshots(&snaps, &dir.path().join("config.yaml"), |_| {
            Err("boom".to_string())
        });
        assert_eq!(out.wheel, None);
        assert_eq!(out.failed.len(), 1);
    }
}
