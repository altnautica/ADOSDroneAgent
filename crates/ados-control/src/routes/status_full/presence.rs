//! The sensing capability flags in `/api/status/full`: `clock`, `sensor_hub`,
//! `gnss` and `capture_storage`. Each is true only when its service is
//! enabled for this profile and its binary, hardware or configuration marker
//! (or a fresh sidecar) is present, so Mission Control shows a tab only for a
//! lane that can work.

use std::path::Path;
use std::time::SystemTime;

use ados_config::capture_storage::SIDECAR_MAX_AGE;
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;

use crate::routes::sensing::run_relative;
use crate::routes::sensing::storage::CAPTURE_PROFILES;

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

/// The four flags, flattened onto the top level of the status body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct PresenceFlags {
    pub clock: bool,
    pub sensor_hub: bool,
    pub gnss: bool,
    pub capture_storage: bool,
}

/// The four flags for `profile`, in either the wire form (`ground-station`,
/// what `/api/status/full` resolves) or the unit-table form (`ground_station`).
pub(crate) fn presence_flags(
    profile: &str,
    dirs: &PresenceDirs<'_>,
    now: SystemTime,
) -> PresenceFlags {
    let gate = profile.replace('-', "_");
    let profile = gate.as_str();
    // The unit execs /bin/true until its best-effort binary lands, so the
    // profile alone does not make a clock.
    let clock = unit_on("ados-clock", profile) && dirs.bin.join("ados-clock").is_file();
    let sensor_hub = unit_on("ados-sensorhub", profile)
        && (dirs.run.join("sensor-hub-present").exists()
            || dirs.etc.join("sensorhub-enabled").exists());
    let gnss = ados_protocol::node_presence::gnss_present(profile, dirs.run, now);
    // The same window the session admission check trusts the volume over.
    let capture_storage = CAPTURE_PROFILES.contains(&profile)
        && fresh(
            &dirs.run.join(run_relative("capture-storage")),
            SIDECAR_MAX_AGE,
            now,
        );
    PresenceFlags {
        clock,
        sensor_hub,
        gnss,
        capture_storage,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::sensing::FRESH;
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
        let p = presence_flags(profile, &dirs, now);
        vec![p.clock, p.sensor_hub, p.gnss, p.capture_storage]
    }

    fn flags(profile: &str, f: &Fixture) -> Vec<bool> {
        flags_at(profile, f, SystemTime::now())
    }

    fn write_capture_storage(f: &Fixture) {
        let path = f.run.join(run_relative("capture-storage"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "{}").unwrap();
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
        write_capture_storage(&f);
        assert_eq!(flags("visor", &f), [true; 4]);
        // A drone has no GNSS service; a ground station no hub or capture.
        assert_eq!(flags("drone", &f), [true, true, false, true]);
        assert_eq!(flags("ground_station", &f), [true, false, true, false]);
        // The wire spelling the status route resolves gates the same way.
        assert_eq!(flags("ground-station", &f), [true, false, true, false]);
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
        write_capture_storage(&f);
        let now = SystemTime::now();
        assert!(flags_at("drone", &f, now + SIDECAR_MAX_AGE - Duration::from_secs(5))[3]);
        assert!(!flags_at("drone", &f, now + SIDECAR_MAX_AGE + Duration::from_secs(5))[3]);
    }
}
