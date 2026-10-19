//! The sensing capability flags in `/api/status/full`: `clock`, `sensor_hub`,
//! `gnss` and `capture_storage`. Each is true only when its service is
//! enabled for this profile and its hardware (or configuration) marker is
//! present, so Mission Control shows a tab only for a lane that can work.

use std::path::Path;

use serde_json::{Map, Value};

use crate::routes::sensing::storage::CAPTURE_PROFILES;

fn unit_on(unit: &str, profile: &str) -> bool {
    ados_protocol::units::find(unit).is_some_and(|u| u.on_profile(profile))
}

/// The four flags for `profile`, given the run and config directories.
pub(crate) fn presence_flags(
    profile: &str,
    run_dir: &Path,
    etc_dir: &Path,
) -> [(&'static str, bool); 4] {
    let clock = unit_on("ados-clock", profile);
    let sensor_hub = unit_on("ados-sensorhub", profile)
        && (run_dir.join("sensor-hub-present").exists()
            || etc_dir.join("sensorhub-enabled").exists());
    // ados-gnss writes its sidecar only when receivers are configured (it
    // exits idle otherwise), so the file is the configuration marker.
    let gnss = unit_on("ados-gnss", profile) && run_dir.join("gnss.json").is_file();
    let capture_storage =
        CAPTURE_PROFILES.contains(&profile) && run_dir.join("capture-storage.json").is_file();
    [
        ("clock", clock),
        ("sensor_hub", sensor_hub),
        ("gnss", gnss),
        ("capture_storage", capture_storage),
    ]
}

pub(super) fn insert_presence_flags(
    payload: &mut Map<String, Value>,
    profile: &str,
    run_dir: &Path,
    etc_dir: &Path,
) {
    for (k, v) in presence_flags(profile, run_dir, etc_dir) {
        payload.insert(k.to_string(), Value::Bool(v));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(profile: &str, run: &Path, etc: &Path) -> Vec<bool> {
        presence_flags(profile, run, etc)
            .iter()
            .map(|f| f.1)
            .collect()
    }

    #[test]
    fn flags_follow_profile_gates_and_markers() {
        let dir = tempfile::tempdir().unwrap();
        let (run, etc) = (dir.path().join("run"), dir.path().join("etc"));
        std::fs::create_dir_all(&run).unwrap();
        std::fs::create_dir_all(&etc).unwrap();
        // Nothing present: only the clock follows the profile alone.
        assert_eq!(flags("visor", &run, &etc), [true, false, false, false]);
        std::fs::write(run.join("sensor-hub-present"), "").unwrap();
        std::fs::write(run.join("gnss.json"), "{}").unwrap();
        std::fs::write(run.join("capture-storage.json"), "{}").unwrap();
        assert_eq!(flags("visor", &run, &etc), [true; 4]);
        // A drone has no GNSS service; a ground station no hub or capture.
        assert_eq!(flags("drone", &run, &etc), [true, true, false, true]);
        assert_eq!(
            flags("ground_station", &run, &etc),
            [true, false, true, false]
        );
        // A workstation runs none of these lanes.
        assert_eq!(flags("workstation", &run, &etc), [false; 4]);
    }
}
