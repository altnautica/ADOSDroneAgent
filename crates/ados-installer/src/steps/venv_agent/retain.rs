//! Agent wheel retention for rollback.
//!
//! Every successfully installed agent package is kept as its wheel under
//! [`ROLLBACK_DIR`] (the verified release wheel on the stable channel, a wheel
//! built from the installed tree on edge). Before an upgrade installs a new
//! version, the retained wheel of the version it replaces is recorded as this
//! run's wheel snapshot, so a failed run can reinstall it. Only the wheels of
//! the previous and the current version are kept.
//!
//! The file keeps its real wheel name (`ados_drone_agent-<ver>-py3-none-any.whl`)
//! because pip refuses to install a wheel whose file name is not a wheel name.

use std::path::{Path, PathBuf};

use crate::rollback::snapshots::{record_snapshot, KIND_WHEEL};
use ados_term::exec;

/// File-name prefix of every agent wheel.
const WHEEL_PREFIX: &str = "ados_drone_agent-";

/// The version an agent wheel file name carries, or `None` for any other file.
pub fn wheel_version(file_name: &str) -> Option<&str> {
    let rest = file_name.strip_prefix(WHEEL_PREFIX)?;
    if !rest.ends_with(".whl") {
        return None;
    }
    rest.split('-').next().filter(|v| !v.is_empty())
}

/// Every agent wheel in `dir`, as `(path, version)`.
fn wheels_in(dir: &Path) -> Vec<(PathBuf, String)> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    read.flatten()
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            let version = wheel_version(&name)?.to_string();
            Some((e.path(), version))
        })
        .collect()
}

/// The retained wheel of `version` in `dir`.
pub fn retained_wheel(dir: &Path, version: &str) -> Option<PathBuf> {
    wheels_in(dir)
        .into_iter()
        .find(|(_, v)| v == version)
        .map(|(p, _)| p)
}

/// Record the retained wheel of `previous` (the version this run replaces) as
/// the run's wheel snapshot in `record`. Logged when there is none to record.
pub fn record_previous(dir: &Path, record: &Path, previous: Option<&str>) {
    let Some(version) = previous else {
        return;
    };
    match retained_wheel(dir, version) {
        Some(wheel) => {
            if let Err(e) = record_snapshot(record, KIND_WHEEL, &wheel) {
                tracing::warn!(error = %e, "agent wheel snapshot not recorded");
            }
        }
        None => tracing::info!(
            version,
            "no retained wheel for the installed agent; a failed upgrade cannot restore it"
        ),
    }
}

/// Copy a verified release wheel into `dir`.
pub fn keep_wheel(wheel: &Path, dir: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let name = wheel
        .file_name()
        .ok_or_else(|| std::io::Error::other("wheel path has no file name"))?;
    let dest = dir.join(name);
    std::fs::copy(wheel, &dest)?;
    Ok(dest)
}

/// Build a wheel of the installed source tree `repo` into `dir` with the venv
/// pip `pip`, without dependencies.
pub fn build_and_keep(pip: &str, repo: &Path, dir: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let staging = tempfile::Builder::new()
        .prefix(".build-")
        .tempdir_in(dir)
        .map_err(|e| e.to_string())?;
    let staging_s = staging.path().to_string_lossy().into_owned();
    let repo_s = repo.to_string_lossy().into_owned();
    let res = exec::run(
        pip,
        &["wheel", "--no-deps", "--wheel-dir", &staging_s, &repo_s],
    );
    if !res.success() {
        return Err(format!("pip wheel failed: {}", res.stderr.trim()));
    }
    let (built, _) = wheels_in(staging.path())
        .into_iter()
        .next()
        .ok_or_else(|| "pip wheel produced no agent wheel".to_string())?;
    let dest = dir.join(built.file_name().unwrap_or_default());
    std::fs::rename(&built, &dest).map_err(|e| e.to_string())?;
    Ok(dest)
}

/// Remove every agent wheel in `dir` whose version is not in `keep`.
pub fn prune(dir: &Path, keep: &[&str]) {
    for (path, version) in wheels_in(dir) {
        if keep.contains(&version.as_str()) {
            continue;
        }
        if let Err(e) = std::fs::remove_file(&path) {
            tracing::warn!(path = %path.display(), error = %e, "old agent wheel not removed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wheel(dir: &Path, version: &str) -> PathBuf {
        let p = dir.join(format!("ados_drone_agent-{version}-py3-none-any.whl"));
        std::fs::write(&p, version).unwrap();
        p
    }

    #[test]
    fn the_version_comes_from_the_wheel_name() {
        assert_eq!(
            wheel_version("ados_drone_agent-0.103.0-py3-none-any.whl"),
            Some("0.103.0")
        );
        assert_eq!(wheel_version("ados_drone_agent-0.103.0.tar.gz"), None);
        assert_eq!(wheel_version("other-1.0.0-py3-none-any.whl"), None);
        assert_eq!(wheel_version("this-run"), None);
    }

    #[test]
    fn the_previous_wheel_is_recorded_and_old_ones_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("this-run");
        let old = wheel(dir.path(), "0.101.0");
        let prev = wheel(dir.path(), "0.102.0");
        let cur = wheel(dir.path(), "0.103.0");

        record_previous(dir.path(), &record, Some("0.102.0"));
        assert_eq!(
            crate::rollback::snapshots::read_run_snapshots(&record).wheel,
            Some(prev.clone())
        );

        prune(dir.path(), &["0.102.0", "0.103.0"]);
        assert!(!old.exists());
        assert!(prev.exists());
        assert!(cur.exists());
        assert!(record.exists(), "the run record is not a wheel");
    }

    #[test]
    fn keep_wheel_copies_under_its_wheel_name() {
        let dir = tempfile::tempdir().unwrap();
        let src = wheel(dir.path(), "0.103.0");
        let kept = keep_wheel(&src, &dir.path().join("rollback")).unwrap();
        assert_eq!(kept.file_name(), src.file_name());
        assert_eq!(
            retained_wheel(&dir.path().join("rollback"), "0.103.0"),
            Some(kept)
        );
    }
}
