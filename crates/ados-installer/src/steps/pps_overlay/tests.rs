use std::cell::Cell;
use std::path::Path;

use super::*;

fn profile_pps(stem: &str) -> (String, i64) {
    let profiles = load_profiles();
    let p = profiles.iter().find(|p| p.stem == stem).expect(stem);
    let gpio = p.gnss.as_ref().and_then(|g| g.pps_gpio).expect("pps_gpio");
    (p.soc.clone(), gpio)
}

#[test]
fn rock_5b_plus_pps_lands_on_gpio3_a4() {
    let (soc, gpio) = profile_pps("rock-5b-plus");
    assert_eq!(gpio, 100);
    let dts = pps_overlay_dts(&soc, gpio).expect("rockchip overlay");
    assert!(dts.starts_with("/dts-v1/;\n/plugin/;\n"), "{dts}");
    assert!(dts.contains("target-path = \"/\";"), "{dts}");
    assert!(dts.contains("compatible = \"pps-gpio\";"), "{dts}");
    assert!(
        dts.contains("gpios = <&gpio3 4 0>; /* GPIO3_A4, GPIO_ACTIVE_HIGH */"),
        "{dts}"
    );
    assert!(dts.contains("Not yet validated on hardware"), "{dts}");
}

#[test]
fn rock_5c_pps_lands_on_gpio1_a5() {
    let (soc, gpio) = profile_pps("rock-5c-lite");
    assert_eq!(gpio, 37);
    let dts = pps_overlay_dts(&soc, gpio).expect("rockchip overlay");
    assert!(
        dts.contains("gpios = <&gpio1 5 0>; /* GPIO1_A5, GPIO_ACTIVE_HIGH */"),
        "{dts}"
    );
}

#[test]
fn bank_and_line_split_and_range() {
    assert_eq!(rockchip_bank_line(0), Some((0, 0)));
    assert_eq!(rockchip_bank_line(159), Some((4, 31)));
    assert_eq!(rockchip_bank_line(160), None);
    assert_eq!(rockchip_bank_line(-1), None);
    assert_eq!(rockchip_pin_name(3, 4), "GPIO3_A4");
    assert_eq!(rockchip_pin_name(4, 31), "GPIO4_D7");
    assert_eq!(pps_overlay_dts("RK3588", 160), None);
}

#[test]
fn pi_uses_the_stock_overlay_not_a_generated_one() {
    assert_eq!(soc_family("BCM2712"), Some(SocFamily::Broadcom));
    assert_eq!(pps_overlay_dts("BCM2712", 18), None);
    assert_eq!(pi_pps_line(18), "dtoverlay=pps-gpio,gpiopin=18");

    let cfg = "dtparam=audio=on\n[all]\ndtoverlay=vc4-kms-v3d\n";
    let out = ensure_pi_pps(cfg, 18);
    assert_eq!(out, format!("{cfg}dtoverlay=pps-gpio,gpiopin=18\n"));
    assert_eq!(ensure_pi_pps(&out, 18), out, "idempotent");
}

#[test]
fn pi_line_escapes_a_model_conditional_section() {
    let out = ensure_pi_pps("[cm4]\notg_mode=1", 18);
    assert_eq!(
        out,
        "[cm4]\notg_mode=1\n[all]\ndtoverlay=pps-gpio,gpiopin=18\n"
    );
}

#[test]
fn an_operator_pps_line_is_left_alone() {
    let cfg = "dtoverlay=pps-gpio,gpiopin=4\n";
    assert_eq!(ensure_pi_pps(cfg, 18), cfg);
}

#[test]
fn unknown_soc_family_gets_nothing() {
    assert_eq!(soc_family("A733"), None);
    assert_eq!(pps_overlay_dts("A733", 100), None);
}

#[test]
fn user_overlays_is_extended_once() {
    assert_eq!(
        ensure_user_overlay("verbosity=1\nuser_overlays=foo\n", OVERLAY_NAME),
        "verbosity=1\nuser_overlays=foo ados-pps\n"
    );
    assert_eq!(
        ensure_user_overlay("verbosity=1", OVERLAY_NAME),
        "verbosity=1\nuser_overlays=ados-pps\n"
    );
    assert_eq!(
        ensure_user_overlay("user_overlays=\n", OVERLAY_NAME),
        "user_overlays=ados-pps\n"
    );
    let done = "user_overlays=foo ados-pps\n";
    assert_eq!(ensure_user_overlay(done, OVERLAY_NAME), done);
}

#[test]
fn managed_list_entry_is_registered_once() {
    assert_eq!(
        ensure_managed_entry("a.dtbo", "ados-pps.dtbo").as_deref(),
        Some("a.dtbo\nados-pps.dtbo\n")
    );
    assert_eq!(
        ensure_managed_entry("ados-pps.dtbo\n", "ados-pps.dtbo"),
        None
    );
}

#[test]
fn mechanism_is_detected_from_the_boot_partition() {
    let dir = tempfile::tempdir().unwrap();
    let boot = dir.path();
    assert_eq!(detect_mechanism(boot, true), None);
    std::fs::write(boot.join("armbianEnv.txt"), "verbosity=1\n").unwrap();
    assert_eq!(detect_mechanism(boot, true), Some(BootMechanism::Armbian));
    std::fs::create_dir_all(boot.join("dtbo")).unwrap();
    std::fs::write(boot.join("dtbo/managed.list"), "").unwrap();
    assert_eq!(
        detect_mechanism(boot, true),
        Some(BootMechanism::RadxaManaged)
    );
    // managed.list without u-boot-update cannot regenerate extlinux.conf.
    assert_eq!(detect_mechanism(boot, false), Some(BootMechanism::Armbian));
}

fn radxa_boot(boot: &Path) -> std::path::PathBuf {
    std::fs::create_dir_all(boot.join("dtbo")).unwrap();
    std::fs::create_dir_all(boot.join("extlinux")).unwrap();
    std::fs::write(boot.join("dtbo/managed.list"), "rk3588-uart7-m1.dtbo\n").unwrap();
    let extlinux = boot.join("extlinux/extlinux.conf");
    std::fs::write(&extlinux, format!("label l0\n{}\n", "x".repeat(200))).unwrap();
    extlinux
}

#[test]
fn radxa_enable_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let boot = dir.path();
    let extlinux = radxa_boot(boot);
    let calls = Cell::new(0);
    let regen = || {
        calls.set(calls.get() + 1);
        let body = std::fs::read_to_string(&extlinux).unwrap();
        std::fs::write(
            &extlinux,
            format!("{body}fdtoverlays /dtbo/ados-pps.dtbo\n"),
        )
        .unwrap();
        true
    };

    assert_eq!(enable_radxa(boot, b"dtbo", regen), Ok(Provisioned::Staged));
    assert_eq!(
        std::fs::read(boot.join("dtbo/ados-pps.dtbo")).unwrap(),
        b"dtbo"
    );
    let list = std::fs::read_to_string(boot.join("dtbo/managed.list")).unwrap();
    assert_eq!(list, "rk3588-uart7-m1.dtbo\nados-pps.dtbo\n");
    assert!(boot.join("extlinux/extlinux.conf.ados-bak").is_file());

    assert_eq!(
        enable_radxa(boot, b"dtbo", regen),
        Ok(Provisioned::AlreadyEnabled)
    );
    assert_eq!(calls.get(), 1, "a re-run must not regenerate extlinux.conf");
}

#[test]
fn radxa_failed_regeneration_restores_extlinux() {
    let dir = tempfile::tempdir().unwrap();
    let boot = dir.path();
    let extlinux = radxa_boot(boot);
    let before = std::fs::read_to_string(&extlinux).unwrap();
    let truncate = || {
        std::fs::write(&extlinux, "x").unwrap();
        true
    };
    let err = enable_radxa(boot, b"dtbo", truncate).unwrap_err();
    assert!(err.contains("truncated"), "{err}");
    assert_eq!(std::fs::read_to_string(&extlinux).unwrap(), before);

    let fail = || false;
    assert!(enable_radxa(boot, b"dtbo", fail).is_err());
    assert_eq!(std::fs::read_to_string(&extlinux).unwrap(), before);
}

#[test]
fn armbian_enable_is_idempotent_and_snapshots_first() {
    let dir = tempfile::tempdir().unwrap();
    let boot = dir.path();
    let env = boot.join("armbianEnv.txt");
    std::fs::write(&env, "verbosity=1\noverlays=uart7-m1\n").unwrap();

    assert_eq!(enable_armbian(boot, b"dtbo"), Ok(Provisioned::Staged));
    assert_eq!(
        std::fs::read(boot.join("overlay-user/ados-pps.dtbo")).unwrap(),
        b"dtbo"
    );
    assert_eq!(
        std::fs::read_to_string(&env).unwrap(),
        "verbosity=1\noverlays=uart7-m1\nuser_overlays=ados-pps\n"
    );
    assert_eq!(
        std::fs::read_to_string(boot.join("armbianEnv.txt.ados-bak")).unwrap(),
        "verbosity=1\noverlays=uart7-m1\n"
    );
    assert_eq!(
        enable_armbian(boot, b"dtbo"),
        Ok(Provisioned::AlreadyEnabled)
    );
    // A changed overlay binary is re-installed and needs a reboot to load.
    assert_eq!(enable_armbian(boot, b"dtbo2"), Ok(Provisioned::Staged));
}

#[test]
fn generated_overlays_compile_when_dtc_is_present() {
    if !on_path("dtc") {
        return;
    }
    for stem in ["rock-5b-plus", "rock-5c-lite"] {
        let (soc, gpio) = profile_pps(stem);
        let dtbo = compile_dts(&pps_overlay_dts(&soc, gpio).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(&dtbo[..4], &[0xd0, 0x0d, 0xfe, 0xed], "{stem}: FDT magic");
    }
}
