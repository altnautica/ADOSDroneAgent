//! The pure bodies of the drop-ins and env file the systemd step writes, and
//! the paths it writes them to.

use super::*;

// ─── Drop-in file paths (deployed outside /opt/ados; mirror uninstall.rs) ────

/// Kernel UDP buffer ceiling for the video pipeline (`install_video_sysctl`).
pub(super) const VIDEO_SYSCTL_FILE: &str = "/etc/sysctl.d/99-ados-video.conf";
/// Kernel freeze-resilience sysctls (`install_power_hardening` step 7): make the
/// kernel reboot on a fatal fault instead of hanging dead, so a hard lockup
/// recovers itself (with the hardware watchdog) and leaves a trace.
pub(super) const RESILIENCE_SYSCTL_FILE: &str = "/etc/sysctl.d/20-ados-resilience.conf";
/// Persistent-journal drop-in (`install_power_hardening` step 7) so a kernel
/// oops / lockup trace survives the reboot instead of being lost with a volatile
/// (RAM) journal — exactly the evidence a hard freeze otherwise erases.
pub(super) const JOURNALD_PERSIST_CONF: &str =
    "/etc/systemd/journald.conf.d/10-ados-persistent.conf";
/// The on-disk journal directory journald writes to when `Storage=persistent`.
pub(super) const JOURNAL_DIR: &str = "/var/log/journal";
/// NetworkManager WiFi power-save force-off drop-in (`install_power_hardening`).
pub(super) const NM_POWERSAVE_CONF: &str = "/etc/NetworkManager/conf.d/99-ados-wifi-powersave.conf";
/// Fallback udev rule that disables WiFi power-save on every `wlan*` add.
pub(super) const WIFI_POWERSAVE_RULE: &str = "/etc/udev/rules.d/99-ados-wifi-powersave.rules";
/// Broad udev rule that disables USB autosuspend on every USB device.
pub(super) const USB_NO_AUTOSUSPEND_RULE: &str =
    "/etc/udev/rules.d/99-ados-usb-no-autosuspend.rules";
/// tmpfiles drop-in that flips the global USB autosuspend default to "never"
/// early in userspace (belt-and-suspenders to the kernel cmdline).
pub(super) const USB_AUTOSUSPEND_TMPFILES: &str = "/etc/tmpfiles.d/99-ados-usb-autosuspend.conf";
/// The kernel cmdline argument that disables USB autosuspend from the first
/// device enumeration (before the rootfs udev rules can apply).
pub(super) const USB_AUTOSUSPEND_ARG: &str = "usbcore.autosuspend=-1";
/// Stale per-interface Ethernet-EEE rule a prior install may have written; the
/// boot oneshot owns EEE-off now, so any leftover rule is removed.
pub(super) const ETH_NO_EEE_RULE: &str = "/etc/udev/rules.d/99-ados-eth-no-eee.rules";
/// The SSH login banner copied from `data/motd/30-ados`.
pub(super) const MOTD_FILE: &str = "/etc/update-motd.d/30-ados";
/// Static avahi service file advertising the AP-side `_ados._tcp` record.
pub(super) const AVAHI_GS_AP_FILE: &str = "/etc/avahi/services/ados-gs-ap.service";
/// tmpfiles drop-in that ages out the agent's own append-only files on disk.
pub(super) const LOG_RETENTION_TMPFILES: &str = "/etc/tmpfiles.d/99-ados-log-retention.conf";

/// Build the video-pipeline UDP-buffer sysctl drop-in body (pure). It bumps the kernel
/// socket-buffer ceilings so the wfb_rx + fanout + mediamtx UDP sockets can actually
/// allocate the 4 MiB SO_RCVBUF/SO_SNDBUF they request at bind time instead of being
/// clamped to the stock ~208 KiB `net.core.rmem_max`.
pub fn video_sysctl_body() -> String {
    "# ADOS video pipeline UDP buffer ceiling. Allows the wfb_rx +\n\
# video_fanout + mediamtx UDP sockets to actually allocate the\n\
# 4 MiB SO_RCVBUF / SO_SNDBUF they request at bind time. Without\n\
# this, the kernel silently clamps to net.core.rmem_max ~208 KiB\n\
# and bursty FEC frame deliveries drop packets at the kernel.\n\
net.core.rmem_max = 16777216\n\
net.core.wmem_max = 16777216\n\
net.core.rmem_default = 4194304\n\
net.core.wmem_default = 4194304\n"
        .to_string()
}

/// Build the kernel freeze-resilience sysctl drop-in body (pure). The default
/// posture is to HANG on a fatal fault (`kernel.panic = 0`), which strands the
/// box until a human power-cycles it and erases the evidence. These flip it to
/// reboot-and-leave-a-trace, pairing with the hardware watchdog (which catches a
/// total lockup) to also catch what the watchdog cannot: an oops.
///
/// **`hung_task_panic` is deliberately OFF, and set explicitly rather than
/// omitted.** It used to be on, with the reasoning that the kernel-default 120 s
/// timeout meant "a legitimately busy I/O path never false-trips". That
/// reasoning does not survive contact with an SBC writing to an SD card: 120 s
/// in uninterruptible sleep is entirely reachable when storage is saturated, and
/// the resulting reboot lands *during* a write. Slow storage then becomes a
/// damaged filesystem, which is slower still, which trips the timeout again —
/// a self-reinforcing loop that ends in a card that will not boot at all.
///
/// The distinction that matters: an **oops** is a kernel bug and the box is
/// already untrustworthy, so rebooting is right. A **hung task** is nearly
/// always slow hardware, and the box is still correct — it needs to be allowed
/// to finish, not shot. The 120 s timeout is kept precisely because it still
/// emits the "blocked for more than 120 seconds" warning with the offending
/// stack, so the evidence lands in the journal without the reboot.
///
/// The value is written as an explicit `0` rather than dropped from the file so
/// that an upgrade actively reverts a node that already has it on, instead of
/// waiting for a reboot to fall back to the kernel default.
pub fn resilience_sysctl_body() -> String {
    "# ADOS Drone Agent — kernel freeze resilience. Reboot on a fatal fault\n\
# instead of hanging dead, so the box recovers itself (with the hardware\n\
# watchdog) and the trace survives in the journal. Generated by the agent;\n\
# do not edit by hand.\n\
#\n\
# hung_task_panic is OFF on purpose: a blocked task is nearly always slow\n\
# storage, and rebooting mid-write turns that into a damaged filesystem,\n\
# which is slower still and trips the timeout again. The timeout is kept so\n\
# the warning and its stack still reach the journal without the reboot.\n\
kernel.panic = 10\n\
kernel.panic_on_oops = 1\n\
kernel.hung_task_panic = 0\n\
kernel.hung_task_timeout_secs = 120\n"
        .to_string()
}

/// Build the log-retention tmpfiles drop-in body (pure).
///
/// Three of the agent's on-disk outputs had no retention of any kind and grew
/// forever: the per-plugin logs (systemd `StandardOutput=append:`, which no
/// rotation was ever configured for), the audit trail, and operator flight
/// recordings. None of them is a fast writer, so none of them was the cause of
/// the write load on a node — but the failure they lead to is a full root
/// filesystem, and a full root filesystem is how a recoverable problem becomes
/// an unbootable card. Nothing else on the box reclaims them.
///
/// `systemd-tmpfiles` rather than logrotate, deliberately: it is already used
/// for the plugin runtime directory, it is present on any systemd host, and it
/// needs no extra package on a minimal image. `systemd-tmpfiles-clean.timer`
/// runs it daily.
///
/// Ages, not sizes, because these are evidence: the useful question is "how far
/// back can I look", and an age answers it directly while a size cap silently
/// depends on how chatty the box happened to be. Recordings are kept longest
/// because an operator deliberately made them.
pub fn log_retention_tmpfiles_body() -> String {
    "# ADOS Drone Agent — age out the agent's own append-only files.\n\
# systemd-tmpfiles-clean.timer runs this daily. Generated by the agent;\n\
# do not edit by hand.\n\
#\n\
# Per-plugin logs: systemd appends to these with no rotation configured\n\
# anywhere, so without this they grow for the life of the box.\n\
d /var/log/ados/plugins 0750 root root 14d\n\
# The audit trail. Kept longer than the plugin logs: it is a record of who\n\
# did what, not a debugging aid.\n\
d /var/log/ados 0750 root root 90d\n\
# Operator-initiated flight recordings. Kept longest because someone chose\n\
# to make them, but not forever — video fills a card faster than anything\n\
# else here.\n\
d /var/ados/recordings 0750 root root 90d\n"
        .to_string()
}

/// Build the persistent-journal drop-in body (pure). With `Storage=persistent`
/// (and the on-disk journal dir present) journald keeps logs on disk, so a
/// kernel oops / lockup trace survives a hard reset — the exact evidence a
/// volatile (RAM) journal loses on the reboot.
pub fn journald_persist_body() -> String {
    "# ADOS Drone Agent — persistent journal so a kernel oops / lockup trace\n\
# survives the reboot (a volatile journal is lost on a hard reset).\n\
# Generated by the agent; do not edit by hand.\n\
[Journal]\n\
Storage=persistent\n\
SystemMaxUse=300M\n\
RuntimeMaxUse=64M\n"
        .to_string()
}

/// Build the NetworkManager WiFi power-save drop-in body (pure). `2` forces
/// power-save OFF for every managed connection (`install_power_hardening` step 1).
pub fn nm_powersave_body() -> String {
    "# ADOS: force WiFi power-save OFF for every managed connection so the\n\
# management link and any WiFi uplink never park the radio.\n\
# 2 = disable power save.\n\
[connection]\n\
wifi.powersave = 2\n"
        .to_string()
}

/// Build the fallback WiFi-power-save udev rule body (pure), prefixed with the resolved
/// `iw` path when one is known (so the RUN+= line is absolute) and an inline `/bin/sh
/// -c 'iw ...'` fallback otherwise.
pub fn wifi_powersave_rule_body(iw_bin: Option<&str>) -> String {
    let mut out =
        String::from("# ADOS: disable WiFi power-save on every wlan* interface as it appears.\n");
    match iw_bin {
        Some(bin) => out.push_str(&format!(
            "ACTION==\"add\", SUBSYSTEM==\"net\", KERNEL==\"wlan*\", RUN+=\"{bin} dev %k set power_save off\"\n"
        )),
        None => out.push_str(
            "ACTION==\"add\", SUBSYSTEM==\"net\", KERNEL==\"wlan*\", RUN+=\"/bin/sh -c 'iw dev %k set power_save off'\"\n",
        ),
    }
    out
}

/// Build the broad USB-no-autosuspend udev rule body (pure). Pins
/// `power/control=on` for every USB device so the RTL8812EU WFB dongle, the
/// management WiFi, and a USB modem do not park on the bus
/// (`install_power_hardening` step 3).
pub fn usb_no_autosuspend_rule_body() -> String {
    "# ADOS: disable USB autosuspend on every USB device. Keeps the WFB radio,\n\
# the management WiFi dongle, and a cellular modem from parking on the bus.\n\
ACTION==\"add\", SUBSYSTEM==\"usb\", ATTR{power/control}=\"on\"\n"
        .to_string()
}

/// Build the USB-autosuspend tmpfiles drop-in body (pure). Flips the global
/// usbcore autosuspend default to never (-1) early in userspace, so any device
/// that enumerates after the rootfs comes up cannot autosuspend. The kernel
/// cmdline ([`disable_usb_autosuspend_cmdline`]) covers the earlier initramfs
/// window this rule cannot reach.
pub fn usb_autosuspend_tmpfiles_body() -> String {
    "# ADOS: flip the global USB autosuspend default to never (-1) early in\n\
# userspace. Belt-and-suspenders to the usbcore.autosuspend=-1 kernel cmdline,\n\
# which covers the initramfs enumeration window this rule cannot reach.\n\
w /sys/module/usbcore/parameters/autosuspend - - - - -1\n"
        .to_string()
}

/// Add `arg` to an Armbian `armbianEnv.txt` body via the `extraargs=` line.
/// Returns the rewritten body, or `None` when `arg` is already present (the
/// file is left untouched). Pure + unit-testable.
pub fn armbian_extraargs_with(content: &str, arg: &str) -> Option<String> {
    if content.split_whitespace().any(|t| t == arg) {
        return None;
    }
    let mut out = String::new();
    let mut had = false;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("extraargs=") {
            had = true;
            let rest = rest.trim();
            if rest.is_empty() {
                out.push_str(&format!("extraargs={arg}\n"));
            } else {
                out.push_str(&format!("extraargs={rest} {arg}\n"));
            }
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !had {
        out.push_str(&format!("extraargs={arg}\n"));
    }
    Some(out)
}

/// Add `arg` to a single-line Raspberry-Pi `cmdline.txt` body. Returns the
/// rewritten body, or `None` when `arg` is already present. Pure.
pub fn cmdline_txt_with(content: &str, arg: &str) -> Option<String> {
    if content.split_whitespace().any(|t| t == arg) {
        return None;
    }
    Some(format!("{} {arg}\n", content.trim_end()))
}

/// Add `arg` to the first `APPEND`/`append` line of an extlinux config body.
/// Returns the rewritten body, or `None` when `arg` is already present or no
/// APPEND line exists. Pure.
pub fn extlinux_append_with(content: &str, arg: &str) -> Option<String> {
    if content.split_whitespace().any(|t| t == arg) {
        return None;
    }
    let mut out = String::new();
    let mut patched = false;
    for line in content.lines() {
        let t = line.trim_start();
        if !patched && (t.starts_with("append ") || t.starts_with("APPEND ")) {
            out.push_str(line);
            out.push(' ');
            out.push_str(arg);
            out.push('\n');
            patched = true;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if patched {
        Some(out)
    } else {
        None
    }
}

/// Build the `/etc/ados/env` file body (pure). `device_id` is read from
/// `/etc/ados/device-id`; empty when not yet minted (it is, by this step). The
/// state IPC wire is unconditionally msgpack (v2), so no selector env var is
/// written.
pub fn env_file_body(device_id: &str) -> String {
    format!(
        "ADOS_DEVICE_ID={device_id}\n\
ADOS_CONFIG={CONFIG_DIR}/config.yaml\n\
ADOS_RUN_DIR=/run/ados\n"
    )
}
