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
}
