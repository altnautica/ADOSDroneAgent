//! The sensing capability flags in `/api/status/full`: `clock`, `sensor_hub`,
//! `gnss` and `capture_storage`. Each is true only when its service is
//! enabled for this profile and its binary, hardware or configuration marker
//! (or a fresh sidecar) is present, so Mission Control shows a tab only for a
//! lane that can work.

use std::path::Path;
use std::time::SystemTime;

use ados_config::capture_storage::SIDECAR_MAX_AGE;
use serde_json::{Map, Value};

use crate::routes::sensing::storage::CAPTURE_PROFILES;
use crate::routes::sensing::FRESH;

fn unit_on(unit: &str, profile: &str) -> bool {
    ados_protocol::units::find(unit).is_some_and(|u| u.on_profile(profile))
}

/// Whether `path` holds a JSON sidecar younger than `max_age` at `now`.
fn fresh(path: &Path, max_age: std::time::Duration, now: SystemTime) -> bool {
    ados_protocol::sidecar::read_fresh_at::<Value>(path, Some(max_age), now).is_fresh()
}

/// Where the flags look.
pub(crate) struct PresenceDirs<'a> {
    pub run: &'a Path,
    pub etc: &'a Path,
    /// The native service binaries (`/opt/ados/bin`).
    pub bin: &'a Path,
}

/// The four flags for `profile`.
pub(crate) fn presence_flags(
    profile: &str,
    dirs: &PresenceDirs<'_>,
    now: SystemTime,
) -> [(&'static str, bool); 4] {
    // The unit execs /bin/true until its best-effort binary lands, so the
    // profile alone does not make a clock.
    let clock = unit_on("ados-clock", profile) && dirs.bin.join("ados-clock").is_file();
    let sensor_hub = unit_on("ados-sensorhub", profile)
        && (dirs.run.join("sensor-hub-present").exists()
            || dirs.etc.join("sensorhub-enabled").exists());
    // ados-gnss writes its sidecar every second only while receivers are
    // configured (it exits idle otherwise); a file left by a stopped service
    // goes stale, so only a fresh one marks the lane as working.
    let gnss = unit_on("ados-gnss", profile) && fresh(&dirs.run.join("gnss.json"), FRESH, now);
    // The same window the session admission check trusts the volume over.
    let capture_storage = CAPTURE_PROFILES.contains(&profile)
        && fresh(&dirs.run.join("capture-storage.json"), SIDECAR_MAX_AGE, now);
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
    dirs: &PresenceDirs<'_>,
) {
    for (k, v) in presence_flags(profile, dirs, SystemTime::now()) {
        payload.insert(k.to_string(), Value::Bool(v));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;

    struct Fixture {
        _dir: tempfile::TempDir,
        run: PathBuf,
        etc: PathBuf,
        bin: PathBuf,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let (run, etc, bin) = (
            dir.path().join("run"),
            dir.path().join("etc"),
            dir.path().join("bin"),
        );
        for d in [&run, &etc, &bin] {
            std::fs::create_dir_all(d).unwrap();
        }
        Fixture {
            _dir: dir,
            run,
            etc,
            bin,
        }
    }

    fn flags_at(profile: &str, f: &Fixture, now: SystemTime) -> Vec<bool> {
        let dirs = PresenceDirs {
            run: &f.run,
            etc: &f.etc,
            bin: &f.bin,
        };
        presence_flags(profile, &dirs, now)
            .iter()
            .map(|x| x.1)
            .collect()
    }

    fn flags(profile: &str, f: &Fixture) -> Vec<bool> {
        flags_at(profile, f, SystemTime::now())
    }

    #[test]
    fn flags_follow_profile_gates_and_markers() {
        let f = fixture();
        // Nothing present, not even the clock binary.
        assert_eq!(flags("visor", &f), [false; 4]);
        std::fs::write(f.bin.join("ados-clock"), "").unwrap();
        assert_eq!(flags("visor", &f), [true, false, false, false]);
        std::fs::write(f.run.join("sensor-hub-present"), "").unwrap();
        std::fs::write(f.run.join("gnss.json"), "{}").unwrap();
        std::fs::write(f.run.join("capture-storage.json"), "{}").unwrap();
        assert_eq!(flags("visor", &f), [true; 4]);
        // A drone has no GNSS service; a ground station no hub or capture.
        assert_eq!(flags("drone", &f), [true, true, false, true]);
        assert_eq!(flags("ground_station", &f), [true, false, true, false]);
        // A workstation runs none of these lanes, binary or not.
        assert_eq!(flags("workstation", &f), [false; 4]);
    }

    #[test]
    fn a_stale_gnss_sidecar_is_not_presence() {
        let f = fixture();
        std::fs::write(f.run.join("gnss.json"), "{}").unwrap();
        let later = SystemTime::now() + FRESH + Duration::from_secs(5);
        assert!(!flags_at("ground_station", &f, later)[2]);
        // Unparseable is not presence either.
        std::fs::write(f.run.join("gnss.json"), "not json").unwrap();
        assert!(!flags("ground_station", &f)[2]);
    }

    #[test]
    fn capture_storage_needs_a_sidecar_younger_than_thirty_seconds() {
        let f = fixture();
        std::fs::write(f.run.join("capture-storage.json"), "{}").unwrap();
        let now = SystemTime::now();
        assert!(flags_at("drone", &f, now + SIDECAR_MAX_AGE - Duration::from_secs(5))[3]);
        assert!(!flags_at("drone", &f, now + SIDECAR_MAX_AGE + Duration::from_secs(5))[3]);
    }
}
