//! Path constants + architecture / OS probe.
//!
//! These are the canonical on-disk locations the installer reads and writes.
//! They mirror the bash installer's layout (`/opt/ados`, `/etc/ados`,
//! `/var/lib/ados`) so a Rust-driven install lands files exactly where the
//! running agent + the bash `--upgrade` path already expect them.

/// Agent install root — venv, bins, and persisted source live under here.
pub const INSTALL_DIR: &str = "/opt/ados";
/// Prebuilt service binaries (one file per Rust service).
pub const BIN_DIR: &str = "/opt/ados/bin";
/// Python virtualenv hosting the agent package + the ecosystem layers.
pub const VENV_DIR: &str = "/opt/ados/venv";
/// Operator config + identity (config.yaml, profile.conf, pairing, device-id).
pub const CONFIG_DIR: &str = "/etc/ados";
/// Mutable agent state (install-result, checkpoints, peripherals).
pub const STATE_DIR: &str = "/var/lib/ados";
/// Per-step `<name>.done` markers so an interrupted install resumes.
pub const CHECKPOINT_DIR: &str = "/var/lib/ados/install-checkpoints";
/// The machine-readable install outcome the heartbeat + GCS consume.
pub const RESULT_PATH: &str = "/var/lib/ados/install-result.json";
/// The top-level systemd unit the install starts and health-gates on.
pub const SERVICE_NAME: &str = "ados-supervisor";
/// Persisted source-tree copy the bash installer leaves behind so an
/// `--upgrade` invoked outside a fresh clone still finds the unit files,
/// udev rules, and driver scripts. The Rust installer's downstream steps fall
/// back to this (then `INSTALL_DIR/repo`) when `ctx.source_dir` is `None`.
pub const PERSISTED_SOURCE_DIR: &str = "/opt/ados/source";
/// The device-id file: a normalized 12-hex string (no dashes), never rewritten.
pub const DEVICE_ID_FILE: &str = "/etc/ados/device-id";
/// On-disk profile selector read by the agent + the bash `resolve_profile`.
pub const PROFILE_CONF: &str = "/etc/ados/profile.conf";
/// The operator config the agent reads on boot.
pub const CONFIG_YAML: &str = "/etc/ados/config.yaml";
/// Pairing material written by `--pair CODE`.
pub const PAIRING_JSON: &str = "/etc/ados/pairing.json";
/// Cloud-relay endpoint baked into the default config's `pairing.convex_url`.
pub const CONVEX_URL: &str = "https://convex-site.altnautica.com";

/// Resolve the source repo dir, mirroring the bash `SYSTEMD_SRC_DIR` /
/// driver-script resolution: prefer the path the clone recorded (`recorded`,
/// i.e. `ctx.source_dir`), then the persisted `/opt/ados/source`, then
/// `INSTALL_DIR/repo`. Returns the first that exists, or `None`.
pub fn resolve_source_dir(recorded: Option<&std::path::Path>) -> Option<std::path::PathBuf> {
    use std::path::PathBuf;
    let candidates: Vec<PathBuf> = [
        recorded.map(PathBuf::from),
        Some(PathBuf::from(PERSISTED_SOURCE_DIR)),
        Some(PathBuf::from(format!("{INSTALL_DIR}/repo"))),
    ]
    .into_iter()
    .flatten()
    .collect();
    candidates.into_iter().find(|p| p.is_dir())
}

/// The on-disk markers that, taken together, mean "the agent is already
/// installed on this box". Used by the install-presence probe so a bare
/// `--pair CODE` on an installed agent can run a fast re-pair instead of the
/// full fresh-install chain.
///
/// All three must be present: the venv interpreter (the agent package lives
/// there), the deployed supervisor unit (the install reached the systemd step),
/// and the persisted device identity (the install reached config/identity). A
/// partial install with only one or two of these is treated as NOT installed,
/// so a re-pair never runs against a half-provisioned box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstallMarkers {
    /// `/opt/ados/venv/bin/python` exists.
    pub venv_python: bool,
    /// A deployed `ados-supervisor.service` unit file is on disk.
    pub supervisor_unit: bool,
    /// `/etc/ados/device-id` exists and is non-empty.
    pub device_id: bool,
}

/// Pure: are all the install markers present? Kept separate from the probe so a
/// unit test can exercise the all-or-nothing rule without touching disk.
pub fn install_present(markers: InstallMarkers) -> bool {
    markers.venv_python && markers.supervisor_unit && markers.device_id
}

/// The unit-file locations a deployed supervisor unit may live in (the
/// `/etc/systemd/system` drop-in the installer writes, or a packaged unit).
const SUPERVISOR_UNIT_DIRS: &[&str] = &["/etc/systemd/system", "/usr/lib/systemd/system"];

/// Probe the running box for an existing install. Reads only the on-disk
/// markers ([`InstallMarkers`]); does NOT contact systemd or the agent, so it is
/// cheap and safe to call before the run mode is resolved. Returns true only
/// when every marker is present (see [`install_present`]).
pub fn probe_install_present() -> bool {
    use std::path::Path;
    let venv_python = Path::new(&format!("{VENV_DIR}/bin/python")).exists();
    let supervisor_unit = SUPERVISOR_UNIT_DIRS
        .iter()
        .any(|dir| Path::new(dir).join("ados-supervisor.service").is_file());
    let device_id = std::fs::read_to_string(DEVICE_ID_FILE)
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    install_present(InstallMarkers {
        venv_python,
        supervisor_unit,
        device_id,
    })
}

/// Resolved host facts the steps gate on. Kept tiny; richer HAL detection is a
/// later phase that runs the dedicated probe crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvInfo {
    /// Normalized architecture (`aarch64` for aarch64/arm64, else the raw arch).
    pub arch: String,
    /// `std::env::consts::OS` (`linux` on an SBC, `macos` on a dev host).
    pub os: String,
    /// Whether the host arch is one the prebuilt binaries target.
    pub supported_arch: bool,
}

impl EnvInfo {
    /// Probe the running host.
    pub fn probe() -> Self {
        EnvInfo {
            arch: arch().to_string(),
            os: std::env::consts::OS.to_string(),
            supported_arch: is_supported_arch(),
        }
    }
}

/// Normalized architecture string. The prebuilt assets are all `*-aarch64`, so
/// `arm64` (the macOS/Apple-silicon spelling) collapses to `aarch64`; anything
/// else passes through unchanged for reporting.
pub fn arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" | "arm64" => "aarch64",
        other => other,
    }
}

/// True when the running architecture is one the prebuilt binaries target.
/// The agent ships `*-aarch64` assets only.
pub fn is_supported_arch() -> bool {
    arch() == "aarch64"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arch_normalizes_arm64_to_aarch64() {
        // On any host the helper must return one of the canonical spellings;
        // on a real aarch64/arm64 host that is exactly "aarch64".
        let a = arch();
        assert!(!a.is_empty());
        if std::env::consts::ARCH == "arm64" || std::env::consts::ARCH == "aarch64" {
            assert_eq!(a, "aarch64");
            assert!(is_supported_arch());
        }
    }

    #[test]
    fn paths_are_under_the_canonical_roots() {
        assert!(BIN_DIR.starts_with(INSTALL_DIR));
        assert!(VENV_DIR.starts_with(INSTALL_DIR));
        assert!(CHECKPOINT_DIR.starts_with(STATE_DIR));
        assert!(RESULT_PATH.starts_with(STATE_DIR));
    }

    #[test]
    fn probe_is_self_consistent() {
        let e = EnvInfo::probe();
        assert_eq!(e.arch, arch());
        assert_eq!(e.supported_arch, is_supported_arch());
        assert_eq!(e.os, std::env::consts::OS);
    }

    #[test]
    fn install_present_requires_all_markers() {
        let all = InstallMarkers {
            venv_python: true,
            supervisor_unit: true,
            device_id: true,
        };
        assert!(install_present(all));

        // Any single marker missing → not installed (a partial install must
        // never be treated as ready for a fast re-pair).
        assert!(!install_present(InstallMarkers {
            venv_python: false,
            ..all
        }));
        assert!(!install_present(InstallMarkers {
            supervisor_unit: false,
            ..all
        }));
        assert!(!install_present(InstallMarkers {
            device_id: false,
            ..all
        }));
        // Nothing present → not installed.
        assert!(!install_present(InstallMarkers {
            venv_python: false,
            supervisor_unit: false,
            device_id: false,
        }));
    }

    #[test]
    fn resolve_source_dir_prefers_recorded_then_falls_back() {
        // A recorded path that exists wins.
        let dir = tempfile::tempdir().unwrap();
        let got = resolve_source_dir(Some(dir.path()));
        assert_eq!(got.as_deref(), Some(dir.path()));

        // A recorded path that does NOT exist falls through to the canonical
        // fallbacks (neither of which exists on a dev host) → None.
        let missing = dir.path().join("nope");
        // On a real SBC `/opt/ados/source` may exist; this assertion only holds
        // on a host where neither fallback dir is present (the CI/dev case).
        if !std::path::Path::new(PERSISTED_SOURCE_DIR).is_dir()
            && !std::path::Path::new(&format!("{INSTALL_DIR}/repo")).is_dir()
        {
            assert_eq!(resolve_source_dir(Some(&missing)), None);
            assert_eq!(resolve_source_dir(None), None);
        }
    }
}
