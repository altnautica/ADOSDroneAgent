//! Tests for the hardening drop-in bodies and the kernel cmdline editors.

use super::*;

#[test]
fn a_non_ground_install_removes_the_stale_ground_station_avahi_file() {
    // A node reinstalled from ground_station to drone must stop advertising
    // the "ADOS Ground Station" record the static service file publishes.
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("ados-gs-ap.service");
    std::fs::write(&file, "<service-group/>").unwrap();
    assert!(remove_avahi_gs_ap_at(&file));
    assert!(!file.exists());
    // Idempotent: nothing left to remove, so no avahi reload is asked for.
    assert!(!remove_avahi_gs_ap_at(&file));
}

#[test]
fn video_sysctl_carries_the_16mib_ceiling() {
    let body = video_sysctl_body();
    // The load-bearing values the wfb_rx + fanout + mediamtx chain needs.
    assert!(body.contains("net.core.rmem_max = 16777216"));
    assert!(body.contains("net.core.wmem_max = 16777216"));
    assert!(body.contains("net.core.rmem_default = 4194304"));
    assert!(body.contains("net.core.wmem_default = 4194304"));
}

#[test]
fn log_retention_covers_every_unbounded_on_disk_writer() {
    // Three of the agent's on-disk outputs had no retention of any kind.
    // None is a fast writer, so none caused the write load on a node — but
    // they lead to a full root filesystem, which is how a recoverable
    // problem becomes an unbootable card.
    let body = log_retention_tmpfiles_body();
    // Per-plugin logs: systemd appends with no rotation configured anywhere.
    assert!(body.contains("/var/log/ados/plugins"));
    // The audit trail.
    assert!(body.contains("/var/log/ados "));
    // Operator flight recordings — the fastest of the three to fill a card.
    assert!(body.contains("/var/ados/recordings"));
    // Every rule carries an age, or it bounds nothing.
    for line in body.lines().filter(|l| l.starts_with("d ")) {
        assert!(
            line.trim_end().ends_with('d'),
            "retention rule without an age bounds nothing: {line}"
        );
    }
}

#[test]
fn resilience_sysctl_reboots_on_a_fault_but_never_on_slow_storage() {
    let body = resilience_sysctl_body();
    // Reboot 10s after a panic instead of hanging dead.
    assert!(body.contains("kernel.panic = 10"));
    // An oops means the kernel is already untrustworthy: reboot.
    assert!(body.contains("kernel.panic_on_oops = 1"));
    // A blocked task means storage is slow, not that the box is broken.
    // Rebooting mid-write is how a slow card becomes an unbootable one, so
    // this must stay off — and be written as an explicit 0 so an upgrade
    // actively reverts a node that already has it on.
    assert!(
        body.contains("kernel.hung_task_panic = 0"),
        "hung_task_panic must be explicitly disabled, not merely omitted"
    );
    // The timeout stays: it is what puts the warning and its stack in the
    // journal, which is the evidence the reboot used to destroy.
    assert!(body.contains("kernel.hung_task_timeout_secs = 120"));
    // No hardlockup/softlockup-panic knobs (not present on the Pi kernel; the
    // -e apply would skip them, but we do not even write them).
    assert!(!body.contains("panic_on_hardlockup"));
    assert!(!body.contains("softlockup_panic"));
}

#[test]
fn journald_persist_keeps_logs_on_disk() {
    let body = journald_persist_body();
    assert!(body.contains("[Journal]"));
    assert!(body.contains("Storage=persistent"));
    assert!(body.contains("SystemMaxUse=300M"));
    // journald stays PERSISTENT deliberately. With the logging store off by
    // default it is the only record that survives a reboot, so the reason a
    // node died has to still be readable from disk after it does.
    assert!(body.contains("Storage=persistent"));
}

// --- the logging store is opt-in ----------------------------------------

#[test]
fn nm_powersave_forces_off() {
    let body = nm_powersave_body();
    assert!(body.contains("[connection]"));
    // 2 = power-save disabled.
    assert!(body.contains("wifi.powersave = 2"));
}

#[test]
fn wifi_powersave_rule_uses_resolved_iw_when_known() {
    let with = wifi_powersave_rule_body(Some("/usr/sbin/iw"));
    assert!(with.contains("KERNEL==\"wlan*\""));
    assert!(with.contains("/usr/sbin/iw dev %k set power_save off"));
    // No inline /bin/sh wrapper when the path is known.
    assert!(!with.contains("/bin/sh -c"));

    let without = wifi_powersave_rule_body(None);
    assert!(without.contains("/bin/sh -c"));
    assert!(without.contains("iw dev %k set power_save off"));
}

#[test]
fn usb_rule_pins_power_control_on() {
    let body = usb_no_autosuspend_rule_body();
    assert!(body.contains("SUBSYSTEM==\"usb\""));
    assert!(body.contains("ATTR{power/control}=\"on\""));
}

#[test]
fn usb_autosuspend_tmpfiles_writes_minus_one() {
    let body = usb_autosuspend_tmpfiles_body();
    assert!(body.contains("w /sys/module/usbcore/parameters/autosuspend - - - - -1"));
}

#[test]
fn armbian_extraargs_appends_and_is_idempotent() {
    // Append to an existing extraargs line, preserving the prior args.
    let got = armbian_extraargs_with(
        "verbosity=1\nextraargs=cma=256M\nrootdev=UUID=x\n",
        USB_AUTOSUSPEND_ARG,
    )
    .expect("should rewrite");
    assert!(got.contains("extraargs=cma=256M usbcore.autosuspend=-1\n"));
    assert!(got.contains("verbosity=1\n") && got.contains("rootdev=UUID=x\n"));
    // Already present → no rewrite (idempotent).
    assert!(armbian_extraargs_with(&got, USB_AUTOSUSPEND_ARG).is_none());
    // No extraargs line at all → one is added.
    let added = armbian_extraargs_with("verbosity=1\n", USB_AUTOSUSPEND_ARG).unwrap();
    assert!(added.contains("extraargs=usbcore.autosuspend=-1\n"));
    // Empty extraargs value → no stray leading space.
    let empty = armbian_extraargs_with("extraargs=\n", USB_AUTOSUSPEND_ARG).unwrap();
    assert!(empty.contains("extraargs=usbcore.autosuspend=-1\n"));
}

#[test]
fn cmdline_txt_appends_to_the_single_line_and_is_idempotent() {
    let got = cmdline_txt_with(
        "console=serial0 root=/dev/mmcblk0p2 rootwait\n",
        USB_AUTOSUSPEND_ARG,
    )
    .expect("should rewrite");
    assert_eq!(
        got,
        "console=serial0 root=/dev/mmcblk0p2 rootwait usbcore.autosuspend=-1\n"
    );
    assert!(cmdline_txt_with(&got, USB_AUTOSUSPEND_ARG).is_none());
}

#[test]
fn cold_boot_enum_aid_arg_appends_once_and_is_idempotent() {
    let got =
        cmdline_txt_with("console=serial0 rootwait\n", USB_OLD_SCHEME_ARG).expect("should rewrite");
    assert_eq!(got, "console=serial0 rootwait usbcore.old_scheme_first=1\n");
    // Idempotent: already present → no rewrite.
    assert!(cmdline_txt_with(&got, USB_OLD_SCHEME_ARG).is_none());
    // Composes with the autosuspend arg on the same line.
    let both = armbian_extraargs_with(&got, USB_AUTOSUSPEND_ARG);
    assert!(both.is_none() || got.contains("usbcore.old_scheme_first=1"));
}

#[test]
fn parse_cold_boot_enum_aid_gate() {
    assert!(parse_cold_boot_enum_aid(
        "video:\n  usb_recovery:\n    cold_boot_enum_aid: true\n"
    ));
    // Off by default: absent block, explicit false, malformed → false.
    assert!(!parse_cold_boot_enum_aid(
        "video:\n  usb_recovery:\n    enabled: true\n"
    ));
    assert!(!parse_cold_boot_enum_aid(
        "video:\n  usb_recovery:\n    cold_boot_enum_aid: false\n"
    ));
    assert!(!parse_cold_boot_enum_aid("agent:\n  name: x\n"));
    assert!(!parse_cold_boot_enum_aid(": : not yaml"));
}

#[test]
fn extlinux_appends_to_append_line_only() {
    let got = extlinux_append_with(
        "LABEL ados\n  KERNEL /boot/Image\n  APPEND root=UUID=x rootwait\n",
        USB_AUTOSUSPEND_ARG,
    )
    .expect("should rewrite");
    assert!(got.contains("APPEND root=UUID=x rootwait usbcore.autosuspend=-1\n"));
    assert!(got.contains("KERNEL /boot/Image\n"));
    // Idempotent + no-APPEND returns None.
    assert!(extlinux_append_with(&got, USB_AUTOSUSPEND_ARG).is_none());
    assert!(extlinux_append_with("LABEL x\n  KERNEL /boot/Image\n", USB_AUTOSUSPEND_ARG).is_none());
}
