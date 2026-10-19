use std::cell::Cell;

use super::super::{enable_armbian, enable_radxa, ensure_managed_entry, ensure_pi_pps};
use super::*;

#[test]
fn the_pi_line_is_removed_and_nothing_else() {
    let base = "[all]\ndtparam=i2c_arm=on\n";
    let enabled = ensure_pi_pps(base, 18);
    assert_ne!(enabled, base);
    assert_eq!(strip_pi_pps(&enabled), base);
    // Idempotent, and a hand-written pps-gpio line with other options stays.
    assert_eq!(strip_pi_pps(base), base);
    let custom = "dtoverlay=pps-gpio,gpiopin=4,assert_falling_edge=1\n";
    assert_eq!(strip_pi_pps(custom), custom);
}

#[test]
fn only_our_user_overlay_token_goes() {
    assert_eq!(
        strip_user_overlay("verbosity=1\nuser_overlays=foo ados-pps\n", OVERLAY_NAME),
        "verbosity=1\nuser_overlays=foo\n"
    );
    // The line goes when ours was its only overlay.
    assert_eq!(
        strip_user_overlay("verbosity=1\nuser_overlays=ados-pps\n", OVERLAY_NAME),
        "verbosity=1\n"
    );
    let untouched = "verbosity=1\nuser_overlays=foo\n";
    assert_eq!(strip_user_overlay(untouched, OVERLAY_NAME), untouched);
}

#[test]
fn the_managed_entry_and_module_are_deregistered_once() {
    let list = ensure_managed_entry("a.dtbo\n", "ados-pps.dtbo").unwrap();
    assert_eq!(
        strip_managed_entry(&list, "ados-pps.dtbo").as_deref(),
        Some("a.dtbo\n")
    );
    assert_eq!(strip_managed_entry("a.dtbo\n", "ados-pps.dtbo"), None);
    assert_eq!(
        strip_pps_module("panel_mipi\npps-gpio\n").as_deref(),
        Some("panel_mipi\n")
    );
    assert_eq!(strip_pps_module("panel_mipi\n"), None);
}

#[test]
fn radxa_removal_reverses_the_enable_and_regenerates_once() {
    let dir = tempfile::tempdir().unwrap();
    let boot = dir.path();
    std::fs::create_dir_all(boot.join("dtbo")).unwrap();
    std::fs::create_dir_all(boot.join("extlinux")).unwrap();
    std::fs::write(boot.join("dtbo/managed.list"), "rk3588-uart7-m1.dtbo\n").unwrap();
    let extlinux = boot.join("extlinux/extlinux.conf");
    std::fs::write(&extlinux, format!("label l0\n{}\n", "x".repeat(200))).unwrap();
    let calls = Cell::new(0);
    let regen = || {
        calls.set(calls.get() + 1);
        true
    };
    enable_radxa(boot, b"dtbo", regen).unwrap();
    assert_eq!(remove_radxa(boot, regen), Ok(true));
    assert!(!boot.join("dtbo/ados-pps.dtbo").exists());
    assert_eq!(
        std::fs::read_to_string(boot.join("dtbo/managed.list")).unwrap(),
        "rk3588-uart7-m1.dtbo\n"
    );
    assert_eq!(calls.get(), 2);
    // Nothing left to remove: no regeneration.
    assert_eq!(remove_radxa(boot, regen), Ok(false));
    assert_eq!(calls.get(), 2);
}

#[test]
fn armbian_removal_reverses_the_enable() {
    let dir = tempfile::tempdir().unwrap();
    let boot = dir.path();
    let env = boot.join("armbianEnv.txt");
    std::fs::write(&env, "verbosity=1\noverlays=uart7-m1\n").unwrap();
    enable_armbian(boot, b"dtbo").unwrap();
    assert_eq!(remove_armbian(boot), Ok(true));
    assert!(!boot.join("overlay-user/ados-pps.dtbo").exists());
    assert_eq!(
        std::fs::read_to_string(&env).unwrap(),
        "verbosity=1\noverlays=uart7-m1\n"
    );
    assert_eq!(remove_armbian(boot), Ok(false));
}
