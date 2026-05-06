//! Pre-flight probe for the WFB-ng broadcast driver stack.
//!
//! Surfaces three signals that gate the agent's air-side radio
//! capability long before `wfb_tx` is spawned:
//!
//! 1. Whether an RTL8812-class kernel driver is loaded (88XXau / 8812
//!    out-of-tree DKMS variants).
//! 2. Whether the userland `wfb_tx` binary is reachable on a trusted
//!    absolute path.
//! 3. Whether a udev rule for Realtek USB radios is installed (so the
//!    adapter binds correctly when hot-plugged after a fresh boot).
//!
//! The probe is best-effort: a missing /proc, a missing /etc, or a
//! missing /usr/bin all collapse cleanly to `false` without failing the
//! enclosing hardware-check pass. The aggregate `WfbDriverStatus`
//! reflects the pair (driver loaded, binary present): both true → Ok,
//! one missing → Warning, both missing → Error.
//!
//! The udev signal is informational only — many Buildroot rootfs ship
//! the udev rule baked into the initramfs and never write it to
//! `/etc/udev/rules.d/`. A missing udev rule is therefore not an
//! Error condition on its own; the operator can still talk to the
//! adapter once the driver loads.

use serde::Serialize;
use std::path::{Path, PathBuf};

/// Aggregate status of the WFB driver stack. Determines which severity
/// the wizard renders next to the tile.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WfbDriverStatus {
    /// Driver loaded AND `wfb_tx` reachable.
    Ok,
    /// One of the two required signals is missing — agent will fall
    /// back to a restricted mode (bench-only, lab-only, or no radio
    /// pipeline at all depending on which side is missing).
    Warning,
    /// Neither the driver nor the binary is available — the agent has
    /// no radio capability on this host.
    Error,
}

/// Pre-flight readout for the WFB-ng broadcast driver stack. Surfaced
/// in the `/api/v1/setup/hardware-check` response under the
/// `wfb_driver` field so the wizard can render an explicit tile.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WfbDriverCheck {
    /// True when /proc/modules contains a line matching an RTL8812
    /// out-of-tree driver (any `8812*` or `88XXau` token).
    pub rtl8812eu_loaded: bool,
    /// True when the `wfb_tx` userland binary resolves on a trusted
    /// absolute path.
    pub wfb_tx_available: bool,
    /// Best-effort: true when any file under /etc/udev/rules.d/ contains
    /// the Realtek USB vendor id `0bda`. Missing udev rule is not by
    /// itself a fatal condition.
    pub udev_rule_present: bool,
    /// Aggregate severity for the tile render.
    pub status: WfbDriverStatus,
    /// Human-readable summary describing the observed signal pair.
    pub message: String,
}

/// Filesystem roots the probe reads. Defaults map to the live
/// production paths; tests pass a synthetic root with a tempdir-backed
/// `/proc/modules` and `/etc/udev/rules.d/` so the test outcome is
/// independent of the host filesystem.
#[derive(Debug, Clone)]
pub struct ProbeRoots {
    /// Path to a `/proc/modules`-shaped file.
    pub modules_path: PathBuf,
    /// Directory the probe scans for udev rule files.
    pub udev_rules_dir: PathBuf,
    /// Absolute candidate paths the probe walks looking for `wfb_tx`.
    /// First existing entry wins. Empty list means the binary cannot
    /// be located via this probe (and `wfb_tx_available` becomes false).
    pub wfb_tx_candidates: Vec<PathBuf>,
}

impl ProbeRoots {
    /// Live production roots. Used by the running agent.
    pub fn production() -> Self {
        Self {
            modules_path: PathBuf::from("/proc/modules"),
            udev_rules_dir: PathBuf::from("/etc/udev/rules.d"),
            wfb_tx_candidates: vec![
                PathBuf::from("/usr/bin/wfb_tx"),
                PathBuf::from("/usr/local/bin/wfb_tx"),
                PathBuf::from("/bin/wfb_tx"),
            ],
        }
    }
}

/// Run the probe against live production roots.
pub fn check_wfb_driver() -> WfbDriverCheck {
    check_wfb_driver_with(&ProbeRoots::production())
}

/// Run the probe against caller-supplied roots. The unit test suite
/// builds a `ProbeRoots` rooted at a tempdir to validate every
/// permutation without touching the real /proc or /etc.
pub fn check_wfb_driver_with(roots: &ProbeRoots) -> WfbDriverCheck {
    let rtl8812eu_loaded = probe_rtl8812eu_loaded(&roots.modules_path);
    let wfb_tx_available = probe_wfb_tx_available(&roots.wfb_tx_candidates);
    let udev_rule_present = probe_udev_rule_present(&roots.udev_rules_dir);
    let (status, message) = rollup_status(rtl8812eu_loaded, wfb_tx_available, udev_rule_present);
    WfbDriverCheck {
        rtl8812eu_loaded,
        wfb_tx_available,
        udev_rule_present,
        status,
        message,
    }
}

/// Read a `/proc/modules`-shaped file and return true when any line
/// begins with `8812` or `88xxau` (case-insensitive). The kernel
/// modules emitted by the upstream out-of-tree DKMS source land under
/// names like `8812eu`, `88XXau`, `8812au`, all of which match.
///
/// Failure to read the file collapses to `false`. On non-Linux hosts
/// /proc/modules is missing entirely; the probe returns false there
/// rather than panic, which keeps the dev-mac unit tests green.
fn probe_rtl8812eu_loaded(modules_path: &Path) -> bool {
    let raw = match std::fs::read_to_string(modules_path) {
        Ok(s) => s,
        Err(_) => return false,
    };
    for line in raw.lines() {
        // /proc/modules format: "<name> <size> <refcount> <users> ...".
        // The driver name is the first whitespace-separated token.
        let name = match line.split_whitespace().next() {
            Some(n) => n,
            None => continue,
        };
        let lower = name.to_ascii_lowercase();
        if lower.starts_with("8812") || lower.starts_with("88xxau") {
            return true;
        }
    }
    false
}

/// True when the first existing absolute path in `candidates` resolves
/// on disk. The candidate list is closed: the probe never inherits
/// `$PATH`, so a subverted PATH cannot redirect the signal.
fn probe_wfb_tx_available(candidates: &[PathBuf]) -> bool {
    candidates.iter().any(|p| p.exists())
}

/// Best-effort: scan files under the udev rules directory and return
/// true if any of them contain the Realtek vendor id `0bda` (case-
/// insensitive). Failure to read the directory or any individual file
/// collapses to `false` without erroring.
fn probe_udev_rule_present(udev_rules_dir: &Path) -> bool {
    let read = match std::fs::read_dir(udev_rules_dir) {
        Ok(r) => r,
        Err(_) => return false,
    };
    for entry in read.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if raw.to_ascii_lowercase().contains("0bda") {
            return true;
        }
    }
    false
}

/// Map the probe trio into the public `(status, message)` pair the
/// wizard renders. The udev signal participates in the message only,
/// not the rollup, because Buildroot rootfs frequently ship the rule
/// in initramfs without writing it to /etc/udev/rules.d/.
fn rollup_status(
    rtl8812eu_loaded: bool,
    wfb_tx_available: bool,
    udev_rule_present: bool,
) -> (WfbDriverStatus, String) {
    let status = match (rtl8812eu_loaded, wfb_tx_available) {
        (true, true) => WfbDriverStatus::Ok,
        (false, false) => WfbDriverStatus::Error,
        _ => WfbDriverStatus::Warning,
    };
    let message = match status {
        WfbDriverStatus::Ok => {
            if udev_rule_present {
                "RTL8812 driver loaded; wfb_tx reachable; udev rule installed.".to_string()
            } else {
                "RTL8812 driver loaded; wfb_tx reachable; udev rule not visible (Buildroot may bake the rule into initramfs).".to_string()
            }
        }
        WfbDriverStatus::Warning => {
            if !rtl8812eu_loaded {
                "RTL8812 driver not loaded. wfb_tx is present but the kernel will not bind a radio adapter until the driver loads.".to_string()
            } else {
                "wfb_tx binary not found on a trusted path. The driver is loaded but the agent cannot start the broadcast pipeline without the userland tool.".to_string()
            }
        }
        WfbDriverStatus::Error => {
            "Neither the RTL8812 driver nor wfb_tx are available. The agent has no radio capability on this host.".to_string()
        }
    };
    (status, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn empty_roots(dir: &std::path::Path) -> ProbeRoots {
        ProbeRoots {
            modules_path: dir.join("proc-modules"),
            udev_rules_dir: dir.join("udev-rules.d"),
            wfb_tx_candidates: vec![dir.join("absent-wfb_tx")],
        }
    }

    #[test]
    fn rollup_both_missing_is_error() {
        let (s, _) = rollup_status(false, false, false);
        assert_eq!(s, WfbDriverStatus::Error);
    }

    #[test]
    fn rollup_only_driver_is_warning() {
        let (s, _) = rollup_status(true, false, false);
        assert_eq!(s, WfbDriverStatus::Warning);
    }

    #[test]
    fn rollup_only_binary_is_warning() {
        let (s, _) = rollup_status(false, true, false);
        assert_eq!(s, WfbDriverStatus::Warning);
    }

    #[test]
    fn rollup_both_present_is_ok() {
        let (s, _) = rollup_status(true, true, false);
        assert_eq!(s, WfbDriverStatus::Ok);
    }

    #[test]
    fn probe_modules_returns_false_for_missing_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nonexistent");
        assert!(!probe_rtl8812eu_loaded(&path));
    }

    #[test]
    fn probe_modules_matches_8812eu_prefix() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("proc-modules");
        std::fs::write(&path, "ext4 786432 1 - Live 0x0\n8812eu 1843200 0 - Live 0x0\n").unwrap();
        assert!(probe_rtl8812eu_loaded(&path));
    }

    #[test]
    fn probe_modules_matches_88xxau_case_insensitive() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("proc-modules");
        std::fs::write(&path, "88XXau 2097152 0 - Live 0x0\n").unwrap();
        assert!(probe_rtl8812eu_loaded(&path));
    }

    #[test]
    fn probe_modules_ignores_unrelated_drivers() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("proc-modules");
        std::fs::write(&path, "rt2800usb 65536 0 - Live 0x0\nbluetooth 524288 0 - Live 0x0\n")
            .unwrap();
        assert!(!probe_rtl8812eu_loaded(&path));
    }

    #[test]
    fn probe_wfb_tx_returns_true_when_any_candidate_exists() {
        let dir = tempdir().unwrap();
        let real = dir.path().join("wfb_tx");
        std::fs::write(&real, "#!/bin/sh\n").unwrap();
        let roots = vec![dir.path().join("absent"), real];
        assert!(probe_wfb_tx_available(&roots));
    }

    #[test]
    fn probe_wfb_tx_returns_false_when_all_candidates_missing() {
        let dir = tempdir().unwrap();
        let roots = vec![dir.path().join("absent-1"), dir.path().join("absent-2")];
        assert!(!probe_wfb_tx_available(&roots));
    }

    #[test]
    fn probe_udev_returns_false_for_missing_dir() {
        let dir = tempdir().unwrap();
        assert!(!probe_udev_rule_present(&dir.path().join("does-not-exist")));
    }

    #[test]
    fn probe_udev_finds_realtek_vendor_id() {
        let dir = tempdir().unwrap();
        let rules = dir.path().join("udev-rules.d");
        std::fs::create_dir_all(&rules).unwrap();
        std::fs::write(
            rules.join("70-rtl8812.rules"),
            "ACTION==\"add\", SUBSYSTEM==\"usb\", ATTR{idVendor}==\"0bda\", ATTR{idProduct}==\"8812\", RUN+=\"/sbin/modprobe 8812eu\"\n",
        )
        .unwrap();
        assert!(probe_udev_rule_present(&rules));
    }

    #[test]
    fn probe_udev_returns_false_when_no_realtek_rule() {
        let dir = tempdir().unwrap();
        let rules = dir.path().join("udev-rules.d");
        std::fs::create_dir_all(&rules).unwrap();
        std::fs::write(
            rules.join("99-other.rules"),
            "ACTION==\"add\", SUBSYSTEM==\"net\", NAME=\"wlan0\"\n",
        )
        .unwrap();
        assert!(!probe_udev_rule_present(&rules));
    }

    #[test]
    fn full_probe_all_missing_yields_error_status() {
        let dir = tempdir().unwrap();
        let roots = empty_roots(dir.path());
        let result = check_wfb_driver_with(&roots);
        assert!(!result.rtl8812eu_loaded);
        assert!(!result.wfb_tx_available);
        assert!(!result.udev_rule_present);
        assert_eq!(result.status, WfbDriverStatus::Error);
        assert!(!result.message.is_empty());
    }

    #[test]
    fn full_probe_serializes_status_lowercase() {
        let dir = tempdir().unwrap();
        let roots = empty_roots(dir.path());
        let result = check_wfb_driver_with(&roots);
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["status"], serde_json::Value::String("error".into()));
        // Field names use snake_case for wire-compat with the rest of
        // the setup surface.
        assert!(json.get("rtl8812eu_loaded").is_some());
        assert!(json.get("wfb_tx_available").is_some());
        assert!(json.get("udev_rule_present").is_some());
        assert!(json.get("message").is_some());
    }
}
