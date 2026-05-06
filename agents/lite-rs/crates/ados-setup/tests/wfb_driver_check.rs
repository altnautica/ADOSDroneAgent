//! Integration tests for the WFB-ng driver pre-flight tile.
//!
//! Drives `check_wfb_driver_with` against synthetic filesystem roots
//! built from a tempdir so the outcome is independent of the real
//! /proc, /etc, and /usr/bin on the host. Covers the three required
//! permutations from the spec:
//!
//! - All probes return false → status = Error.
//! - Driver loaded + wfb_tx reachable → status = Ok.
//! - Only wfb_tx reachable → status = Warning.
//!
//! Plus the symmetric Warning case (driver loaded, binary absent) and
//! a sanity pass on the udev-rule signal.

use std::path::PathBuf;

use ados_setup::{check_wfb_driver_with, ProbeRoots, WfbDriverStatus};

fn build_roots(
    dir: &std::path::Path,
    driver_loaded: bool,
    binary_present: bool,
    udev_present: bool,
) -> ProbeRoots {
    let modules_path = dir.join("proc-modules");
    if driver_loaded {
        std::fs::write(
            &modules_path,
            "ext4 786432 1 - Live 0x0\n8812eu 1843200 0 - Live 0x0\n",
        )
        .unwrap();
    } else {
        // Write a /proc/modules with only unrelated drivers so the
        // probe sees a real file but no RTL8812-class match.
        std::fs::write(
            &modules_path,
            "ext4 786432 1 - Live 0x0\nbluetooth 524288 0 - Live 0x0\n",
        )
        .unwrap();
    }

    let udev_dir = dir.join("udev-rules.d");
    std::fs::create_dir_all(&udev_dir).unwrap();
    if udev_present {
        std::fs::write(
            udev_dir.join("70-rtl8812.rules"),
            "ACTION==\"add\", SUBSYSTEM==\"usb\", ATTR{idVendor}==\"0bda\", ATTR{idProduct}==\"8812\"\n",
        )
        .unwrap();
    } else {
        std::fs::write(
            udev_dir.join("99-other.rules"),
            "ACTION==\"add\", SUBSYSTEM==\"net\", NAME=\"wlan0\"\n",
        )
        .unwrap();
    }

    let wfb_path = dir.join("wfb_tx");
    let candidates: Vec<PathBuf> = if binary_present {
        std::fs::write(&wfb_path, "#!/bin/sh\n").unwrap();
        vec![wfb_path]
    } else {
        vec![dir.join("absent-wfb_tx")]
    };

    ProbeRoots {
        modules_path,
        udev_rules_dir: udev_dir,
        wfb_tx_candidates: candidates,
    }
}

#[test]
fn all_probes_false_reports_error_status() {
    let dir = tempfile::tempdir().unwrap();
    let roots = build_roots(dir.path(), false, false, false);
    let result = check_wfb_driver_with(&roots);
    assert!(!result.rtl8812eu_loaded);
    assert!(!result.wfb_tx_available);
    assert!(!result.udev_rule_present);
    assert_eq!(result.status, WfbDriverStatus::Error);
    assert!(!result.message.is_empty());
}

#[test]
fn driver_loaded_and_binary_present_reports_ok_status() {
    let dir = tempfile::tempdir().unwrap();
    let roots = build_roots(dir.path(), true, true, true);
    let result = check_wfb_driver_with(&roots);
    assert!(result.rtl8812eu_loaded);
    assert!(result.wfb_tx_available);
    assert!(result.udev_rule_present);
    assert_eq!(result.status, WfbDriverStatus::Ok);
}

#[test]
fn only_binary_available_reports_warning_status() {
    let dir = tempfile::tempdir().unwrap();
    let roots = build_roots(dir.path(), false, true, false);
    let result = check_wfb_driver_with(&roots);
    assert!(!result.rtl8812eu_loaded);
    assert!(result.wfb_tx_available);
    assert_eq!(result.status, WfbDriverStatus::Warning);
    // The message should call out the missing driver, not the missing
    // binary, since the binary is what's actually present.
    assert!(
        result.message.to_lowercase().contains("driver"),
        "warning message should reference the driver: {}",
        result.message
    );
}

#[test]
fn only_driver_loaded_reports_warning_status() {
    let dir = tempfile::tempdir().unwrap();
    let roots = build_roots(dir.path(), true, false, false);
    let result = check_wfb_driver_with(&roots);
    assert!(result.rtl8812eu_loaded);
    assert!(!result.wfb_tx_available);
    assert_eq!(result.status, WfbDriverStatus::Warning);
    assert!(
        result.message.to_lowercase().contains("wfb_tx"),
        "warning message should reference wfb_tx: {}",
        result.message
    );
}

#[test]
fn ok_status_serializes_lowercase_for_wire_compat() {
    let dir = tempfile::tempdir().unwrap();
    let roots = build_roots(dir.path(), true, true, false);
    let result = check_wfb_driver_with(&roots);
    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["status"], serde_json::Value::String("ok".into()));
    // Field shape sanity for older clients reading the body.
    assert!(json.get("rtl8812eu_loaded").is_some());
    assert!(json.get("wfb_tx_available").is_some());
    assert!(json.get("udev_rule_present").is_some());
    assert!(json.get("message").is_some());
}
