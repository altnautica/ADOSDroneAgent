//! The native-vs-packaged runtime badge, computed in process.
//!
//! The long-running services ship as a native binary under `/opt/ados/bin`
//! and, for some, a packaged Python predecessor. The badge rolls the
//! per-service branches up into one label the GCS shows on the node:
//!
//! * `"packaged"`: no native binary is present at all;
//! * `"native"`: every service that applies to the node's profile would run
//!   its native binary;
//! * `"hybrid"`: some native, some not.
//!
//! A port of the Python `ados.core.runtime_mode` (`compute_runtime_mode` and
//! `is_service_native`) with the same per-profile binary lists, so the native
//! front reports the same string with no Python process running. The sweep
//! only stats files; [`current_in`] caches it for [`RUNTIME_MODE_TTL`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// Where the native service binaries are installed (the units exec them here).
pub const DEFAULT_BIN_DIR: &str = "/opt/ados/bin";

/// Where an opt-in or opt-out service's sentinel file lives.
pub const DEFAULT_ETC_DIR: &str = "/etc/ados";

/// How long a computed badge is reused before the binaries are stat-ed again.
pub const RUNTIME_MODE_TTL: Duration = Duration::from_secs(10);

/// Core services on every profile: native whenever the binary is present.
const CORE_BINARIES_UNIVERSAL: &[&str] = &["ados-supervisor", "ados-cloud", "ados-mavlink-router"];

/// Camera-node core (drone, visor): the encode pipeline.
const CORE_BINARIES_CAMERA: &[&str] = &["ados-video"];

/// A service whose branch is chosen by binary presence and, when `flag` is
/// set, a sentinel file under the etc dir. `flag: None` is native-only (the
/// packaged predecessor was deleted). `opt_out` makes native the default and
/// the flag a pin back to packaged.
struct FlagGated {
    name: &'static str,
    flag: Option<&'static str>,
    binaries: &'static [&'static str],
    profiles: &'static [&'static str],
    opt_out: bool,
}

/// The flag-gated services, with the profiles each applies to (hyphen form).
const FLAG_GATED: &[FlagGated] = &[
    FlagGated {
        name: "net",
        flag: None,
        binaries: &["ados-net"],
        profiles: &["ground-station"],
        opt_out: false,
    },
    FlagGated {
        name: "groundlink",
        flag: None,
        binaries: &["ados-groundlink"],
        profiles: &["ground-station"],
        opt_out: false,
    },
    FlagGated {
        name: "plugin-host",
        flag: None,
        binaries: &["ados-plugin-host"],
        profiles: &["drone", "ground-station", "visor"],
        opt_out: false,
    },
    FlagGated {
        name: "hid",
        flag: None,
        binaries: &["ados-pic", "ados-input"],
        profiles: &["ground-station", "visor"],
        opt_out: false,
    },
    FlagGated {
        name: "radio",
        flag: None,
        binaries: &["ados-radio"],
        profiles: &["drone"],
        opt_out: false,
    },
    FlagGated {
        name: "display",
        flag: None,
        binaries: &["ados-display", "ados-display-probe"],
        profiles: &["drone", "ground-station", "visor"],
        opt_out: true,
    },
];

/// The core binaries the installer fetches for `profile`.
fn core_binaries_for(profile: &str) -> Vec<&'static str> {
    let mut list = CORE_BINARIES_UNIVERSAL.to_vec();
    if matches!(profile, "drone" | "visor") {
        list.extend_from_slice(CORE_BINARIES_CAMERA);
    }
    list
}

/// True when the named binary exists and is executable. Total: any error
/// reads as absent.
fn bin_present(bin_dir: &Path, name: &str) -> bool {
    let Ok(meta) = std::fs::metadata(bin_dir.join(name)) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

fn any_native_binary_present(bin_dir: &Path) -> bool {
    CORE_BINARIES_UNIVERSAL
        .iter()
        .chain(CORE_BINARIES_CAMERA)
        .any(|b| bin_present(bin_dir, b))
        || FLAG_GATED
            .iter()
            .flat_map(|svc| svc.binaries.iter())
            .any(|b| bin_present(bin_dir, b))
}

fn flag_set(etc_dir: &Path, flag: &str) -> bool {
    etc_dir.join(flag).exists()
}

/// The service's verdict: whether it would run its native binary.
fn verdict(svc: &FlagGated, bin_dir: &Path, etc_dir: &Path) -> bool {
    let binaries_present = svc.binaries.iter().all(|b| bin_present(bin_dir, b));
    match svc.flag {
        None => binaries_present,
        Some(flag) if svc.opt_out => binaries_present && !flag_set(etc_dir, flag),
        Some(flag) => binaries_present && flag_set(etc_dir, flag),
    }
}

/// True when the named flag-gated service would run its native binary.
/// Unknown services are `false`.
pub fn is_service_native(service: &str, bin_dir: &Path, etc_dir: &Path) -> bool {
    FLAG_GATED
        .iter()
        .find(|svc| svc.name == service)
        .is_some_and(|svc| verdict(svc, bin_dir, etc_dir))
}

/// The badge for a node of wire-form `profile` (`"drone"`, `"visor"`, ...), from
/// the binaries under `bin_dir` and the sentinels under `etc_dir`.
pub fn compute(profile: &str, bin_dir: &Path, etc_dir: &Path) -> &'static str {
    if !any_native_binary_present(bin_dir) {
        return "packaged";
    }
    let core_native = core_binaries_for(profile)
        .iter()
        .all(|b| bin_present(bin_dir, b));
    let gated_native = FLAG_GATED
        .iter()
        .filter(|svc| svc.profiles.contains(&profile))
        .all(|svc| verdict(svc, bin_dir, etc_dir));
    if core_native && gated_native {
        "native"
    } else {
        "hybrid"
    }
}

/// Computed badges by profile and binary directory, with when each was taken.
type ModeCache = HashMap<(String, PathBuf), (Instant, &'static str)>;

static CACHE: LazyLock<Mutex<ModeCache>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// The badge for `profile` against the binaries under `bin_dir`, reused for
/// [`RUNTIME_MODE_TTL`] so a status poll does not stat every binary.
pub fn current_in(profile: &str, bin_dir: &Path) -> &'static str {
    let key = (profile.to_string(), bin_dir.to_path_buf());
    let mut cache = CACHE.lock();
    if let Some(&(at, mode)) = cache.get(&key) {
        if at.elapsed() < RUNTIME_MODE_TTL {
            return mode;
        }
    }
    let mode = compute(profile, bin_dir, Path::new(DEFAULT_ETC_DIR));
    cache.insert(key, (Instant::now(), mode));
    mode
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install(bin_dir: &Path, names: &[&str]) {
        for name in names {
            let path = bin_dir.join(name);
            std::fs::write(&path, b"#!/bin/sh\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
    }

    const DRONE_SET: &[&str] = &[
        "ados-supervisor",
        "ados-cloud",
        "ados-mavlink-router",
        "ados-video",
        "ados-plugin-host",
        "ados-radio",
        "ados-display",
        "ados-display-probe",
    ];

    #[test]
    fn runtime_mode_native_hybrid_packaged() {
        let bin = tempfile::tempdir().unwrap();
        let etc = tempfile::tempdir().unwrap();
        // No binaries at all, and no sidecar anywhere: packaged.
        assert_eq!(compute("drone", bin.path(), etc.path()), "packaged");
        // Some of the drone set: hybrid.
        install(bin.path(), &["ados-supervisor", "ados-cloud"]);
        assert_eq!(compute("drone", bin.path(), etc.path()), "hybrid");
        // The whole drone set: native.
        install(bin.path(), DRONE_SET);
        assert_eq!(compute("drone", bin.path(), etc.path()), "native");
    }

    #[test]
    fn a_binary_the_profile_never_fetches_does_not_hold_it_back() {
        let bin = tempfile::tempdir().unwrap();
        let etc = tempfile::tempdir().unwrap();
        // The ground set: no ados-video and no ados-radio, both drone-only.
        install(
            bin.path(),
            &[
                "ados-supervisor",
                "ados-cloud",
                "ados-mavlink-router",
                "ados-net",
                "ados-groundlink",
                "ados-plugin-host",
                "ados-pic",
                "ados-input",
                "ados-display",
                "ados-display-probe",
            ],
        );
        assert_eq!(compute("ground-station", bin.path(), etc.path()), "native");
        // The same layout is hybrid for a drone, which needs ados-video.
        assert_eq!(compute("drone", bin.path(), etc.path()), "hybrid");
    }

    #[cfg(unix)]
    #[test]
    fn a_non_executable_file_is_not_an_installed_binary() {
        let bin = tempfile::tempdir().unwrap();
        let etc = tempfile::tempdir().unwrap();
        std::fs::write(bin.path().join("ados-cloud"), b"").unwrap();
        assert_eq!(compute("drone", bin.path(), etc.path()), "packaged");
    }

    #[test]
    fn service_native_follows_its_binaries() {
        let bin = tempfile::tempdir().unwrap();
        let etc = tempfile::tempdir().unwrap();
        assert!(!is_service_native("hid", bin.path(), etc.path()));
        install(bin.path(), &["ados-pic"]);
        assert!(!is_service_native("hid", bin.path(), etc.path()));
        install(bin.path(), &["ados-input"]);
        assert!(is_service_native("hid", bin.path(), etc.path()));
        assert!(!is_service_native(
            "no-such-service",
            bin.path(),
            etc.path()
        ));
    }
}
