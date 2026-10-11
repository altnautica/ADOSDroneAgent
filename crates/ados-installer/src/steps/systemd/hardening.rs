//! Cross-profile kernel, power and login hardening: the video sysctl, the
//! power-save and autosuspend drop-ins, the kernel cmdline arguments, the
//! Rockchip ISP mask, the login banner and the ground-station avahi record.

use super::*;

/// The kernel cmdline arg that makes USB enumeration retry the legacy address
/// scheme first. A reversible, opt-in cold-boot aid for a marginal device (e.g.
/// a camera whose port-enable handshake fails once at cold boot and the kernel
/// abandons). Off by default; gated on `video.usb_recovery.cold_boot_enum_aid`.
pub(super) const USB_OLD_SCHEME_ARG: &str = "usbcore.old_scheme_first=1";

/// Append `arg` to the board's kernel cmdline (Armbian `extraargs=`, Pi
/// single-line `cmdline.txt`, or extlinux `APPEND`), board-aware via the same
/// probe order + the pure writers. Idempotent (a no-op when already present).
/// Returns the boot file matched, or `None` when no known cmdline file exists.
/// Takes effect on the next reboot.
///
/// Every write goes through [`crate::steps::boot_config`], so the file gains a
/// KEEP-FIRST `<file>.ados-bak` baseline and a malformed cmdline is recoverable
/// — and an existing pristine snapshot (the display installer's, on a board
/// whose panel overlay lives in the same `extlinux.conf`) is never overwritten.
pub(super) fn apply_cmdline_arg(arg: &str) -> Option<&'static str> {
    // Armbian / many Rockchip boards: the extraargs= line in armbianEnv.txt.
    let armbian = Path::new("/boot/armbianEnv.txt");
    if armbian.is_file() {
        boot_config::edit_boot_config_at(&[armbian], "kernel cmdline arg", |c| {
            armbian_extraargs_with(c, arg).unwrap_or_else(|| c.to_string())
        });
        return Some("/boot/armbianEnv.txt");
    }

    // Raspberry Pi: a single-line cmdline.txt (firmware path first).
    for cmd in ["/boot/firmware/cmdline.txt", "/boot/cmdline.txt"] {
        let p = Path::new(cmd);
        if p.is_file() {
            boot_config::edit_boot_config_at(&[p], "kernel cmdline arg", |c| {
                cmdline_txt_with(c, arg).unwrap_or_else(|| c.to_string())
            });
            return Some(cmd);
        }
    }

    // Generic extlinux: the APPEND line.
    let extlinux = Path::new("/boot/extlinux/extlinux.conf");
    if extlinux.is_file() {
        boot_config::edit_boot_config_at(&[extlinux], "kernel cmdline arg", |c| {
            extlinux_append_with(c, arg).unwrap_or_else(|| c.to_string())
        });
        return Some("/boot/extlinux/extlinux.conf");
    }

    None
}

/// Append `usbcore.autosuspend=-1` to the board's kernel cmdline so USB
/// autosuspend is disabled from the very first device enumeration — before the
/// rootfs udev `power/control=on` rules can apply. Cheap UVC cameras wedge on
/// the kernel-default 2 s autosuspend during the boot-race window the udev rule
/// cannot win; only a cmdline setting is active that early. Board-aware +
/// idempotent. Takes effect on the next reboot.
pub(super) fn disable_usb_autosuspend_cmdline() {
    match apply_cmdline_arg(USB_AUTOSUSPEND_ARG) {
        Some(file) => tracing::info!(
            file,
            arg = USB_AUTOSUSPEND_ARG,
            "usb autosuspend disabled on kernel cmdline (reboot to apply)"
        ),
        None => tracing::info!(
            "no known boot cmdline file found; relying on the udev rule + tmpfiles default for USB autosuspend"
        ),
    }
}

/// Parse `video.usb_recovery.cold_boot_enum_aid` from config.yaml text. Absent /
/// malformed / not-present → false (the aid is an experiment, off by default).
/// Pure.
pub fn parse_cold_boot_enum_aid(text: &str) -> bool {
    #[derive(serde::Deserialize, Default)]
    struct Raw {
        #[serde(default)]
        video: Video,
    }
    #[derive(serde::Deserialize, Default)]
    struct Video {
        #[serde(default)]
        usb_recovery: Option<Rec>,
    }
    #[derive(serde::Deserialize, Default)]
    struct Rec {
        #[serde(default)]
        cold_boot_enum_aid: bool,
    }
    serde_norway::from_str::<Raw>(text)
        .ok()
        .and_then(|r| r.video.usb_recovery)
        .map(|r| r.cold_boot_enum_aid)
        .unwrap_or(false)
}

/// Read the cold-boot-enum-aid opt-in from config.yaml on disk.
pub(super) fn cold_boot_enum_aid_enabled() -> bool {
    match std::fs::read_to_string(crate::env::CONFIG_YAML) {
        Ok(text) => parse_cold_boot_enum_aid(&text),
        Err(_) => false,
    }
}

/// Apply the opt-in cold-boot enumeration aid (`usbcore.old_scheme_first=1`) to
/// the kernel cmdline when the operator has enabled it. Reversible: remove the
/// token from the boot cmdline + reboot. A no-op when the opt-in is off (the
/// default).
pub(super) fn apply_cold_boot_enum_aid_cmdline() {
    if !cold_boot_enum_aid_enabled() {
        return;
    }
    if let Some(file) = apply_cmdline_arg(USB_OLD_SCHEME_ARG) {
        tracing::info!(
            file,
            arg = USB_OLD_SCHEME_ARG,
            "cold-boot usb enumeration aid enabled on kernel cmdline (reboot to apply)"
        );
    }
}

/// Drop the video-pipeline UDP sysctl tuning and apply it now so the running
/// agent picks up the new ceiling on its next socket bind. Idempotent overwrite.
/// Ports 03-kernel.sh `install_video_sysctl`.
pub(super) fn install_video_sysctl() {
    if let Err(e) = std::fs::write(VIDEO_SYSCTL_FILE, video_sysctl_body()) {
        tracing::warn!(error = %e, "writing video sysctl drop-in failed");
        return;
    }
    set_mode(Path::new(VIDEO_SYSCTL_FILE), 0o644);
    // Apply now (best-effort; absent on a stripped container build path).
    let _ = exec::run("sysctl", &["-p", VIDEO_SYSCTL_FILE]);
}

/// Resolve the absolute `iw` path the way `_power_resolve_iw` does: prefer the
/// standard sbin locations, then a PATH lookup.
pub(super) fn resolve_iw_path() -> Option<String> {
    for candidate in ["/usr/sbin/iw", "/sbin/iw"] {
        if Path::new(candidate).exists() {
            return Some(candidate.to_string());
        }
    }
    let which = exec::run("sh", &["-c", "command -v iw"]);
    let trimmed = which.stdout.trim();
    if which.success() && !trimmed.is_empty() {
        Some(trimmed.to_string())
    } else {
        None
    }
}

/// Board-agnostic power hardening: keep WiFi, the WFB radio, and the camera
/// continuously powered. Ports 03b-power.sh `install_power_hardening`:
///
/// 1. NetworkManager WiFi power-save force-off drop-in (+ reload).
/// 2. Fallback WiFi-power-save udev rule (for non-NM-managed `wlan*`).
/// 3. Broad USB-no-autosuspend udev rule.
/// 4. Remove any stale per-interface EEE rule (the boot oneshot owns EEE-off).
/// 5. Deploy `ados-power-reassert.sh` to /opt/ados/bin + enable/start the
///    `ados-power.service` oneshot (the unit file ships in data/systemd and is
///    already deployed by `deploy_units`).
///
/// Sleep-masking + the logind no-sleep drop-in live in the dedicated appliance
/// step (steps/appliance.rs); this function no longer touches sleep.
///
/// Idempotent; applies on the spot. CPU governor is intentionally left untouched.
pub(super) fn install_power_hardening(source: Option<&Path>) {
    // ── 1. WiFi power-save off (NetworkManager) ──
    let nm_present = Path::new("/etc/NetworkManager").is_dir()
        || exec::run_ok("sh", &["-c", "command -v nmcli"]);
    if nm_present {
        if let Some(parent) = Path::new(NM_POWERSAVE_CONF).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::write(NM_POWERSAVE_CONF, nm_powersave_body()).is_ok() {
            set_mode(Path::new(NM_POWERSAVE_CONF), 0o644);
            // Reload NM so the drop-in takes effect without a reboot.
            if !exec::run_ok("nmcli", &["general", "reload"]) {
                let _ = exec::run("systemctl", &["reload", "NetworkManager"]);
            }
        }
    } else {
        tracing::info!("NetworkManager not present; skipping WiFi power-save drop-in");
    }

    // ── 2. WiFi power-save off (fallback udev) ──
    let _ = std::fs::create_dir_all(UDEV_RULES_DIR);
    let iw_bin = resolve_iw_path();
    if std::fs::write(
        WIFI_POWERSAVE_RULE,
        wifi_powersave_rule_body(iw_bin.as_deref()),
    )
    .is_ok()
    {
        set_mode(Path::new(WIFI_POWERSAVE_RULE), 0o644);
    }

    // ── 3. USB autosuspend off (broad) ──
    if std::fs::write(USB_NO_AUTOSUSPEND_RULE, usb_no_autosuspend_rule_body()).is_ok() {
        set_mode(Path::new(USB_NO_AUTOSUSPEND_RULE), 0o644);
    }

    // ── 3b. USB autosuspend off at the kernel level (the boot-race fix). ──
    // The udev rule above pins power/control=on per device, but it cannot win
    // the boot race: a cheap UVC camera enumerates before the rootfs udev is up,
    // inherits the kernel-default 2 s autosuspend, mishandles the resume, and
    // wedges off the bus. Disable autosuspend globally — on the kernel cmdline
    // (active from the first enumeration; takes effect next reboot) and via a
    // tmpfiles default flip (this boot, post-rootfs devices).
    disable_usb_autosuspend_cmdline();
    // Opt-in cold-boot enumeration aid for a marginal device that fails its
    // port-enable once at cold boot (off by default).
    apply_cold_boot_enum_aid_cmdline();
    if std::fs::write(USB_AUTOSUSPEND_TMPFILES, usb_autosuspend_tmpfiles_body()).is_ok() {
        set_mode(Path::new(USB_AUTOSUSPEND_TMPFILES), 0o644);
        let _ = exec::run("systemd-tmpfiles", &["--create", USB_AUTOSUSPEND_TMPFILES]);
    }

    // ── 4. Drop any stale Ethernet-EEE udev rule (the boot oneshot owns it). ──
    let _ = std::fs::remove_file(ETH_NO_EEE_RULE);

    // Reload udev + re-fire on already-bound USB devices so an upgrade applies
    // the new rules without a replug. Deliberately NOT the net subsystem — that
    // would re-run interface-add rules and risk bouncing the wired mgmt link.
    let _ = exec::run("udevadm", &["control", "--reload"]);
    let _ = exec::run(
        "udevadm",
        &["trigger", "--subsystem-match=usb", "--action=change"],
    );

    // Masking the sleep targets + the logind no-sleep drop-in is owned by the
    // dedicated appliance step (steps/appliance.rs), which also quiets desktop
    // background package daemons — so it is not repeated here.

    // ── 5. Boot re-assert oneshot. ──
    // The unit file (data/systemd/ados-power.service) is deployed by
    // deploy_units; here we install the helper script it execs and enable+start
    // the unit. The script lives at scripts/ados-power-reassert.sh in the source
    // tree, which this step requires; there is deliberately no second inline
    // copy to drift from it.
    let bin_dir = format!("{INSTALL_DIR}/bin");
    let _ = std::fs::create_dir_all(&bin_dir);
    let reassert_dst = format!("{bin_dir}/ados-power-reassert.sh");
    let reassert_src = source.map(|s| s.join("scripts/ados-power-reassert.sh"));
    let copied = reassert_src
        .as_deref()
        .filter(|p| p.is_file())
        .map(|p| std::fs::copy(p, &reassert_dst).is_ok())
        .unwrap_or(false);
    if copied {
        set_mode(Path::new(&reassert_dst), 0o755);
        let _ = exec::run("systemctl", &["enable", "ados-power.service"]);
        // Run it now so the knobs are asserted on the current boot too.
        let _ = exec::run("systemctl", &["start", "ados-power.service"]);
    } else {
        tracing::warn!(
            src = ?reassert_src,
            "power re-assert helper missing from the source tree; boot re-assert not installed"
        );
    }

    // ── 6. Kernel freeze resilience. The hardware watchdog (steps/watchdog.rs)
    // catches a total lockup; these catch the cases it cannot — a hung kernel
    // task while PID 1 is still alive to pet the watchdog, or an oops — by
    // turning a hang into a reboot, and keep the trace on disk so a freeze stops
    // erasing its own evidence.
    if std::fs::write(RESILIENCE_SYSCTL_FILE, resilience_sysctl_body()).is_ok() {
        set_mode(Path::new(RESILIENCE_SYSCTL_FILE), 0o644);
        // `-e` ignores any knob this kernel lacks (e.g. an absent hardlockup
        // detector) instead of failing the whole apply.
        let _ = exec::run("sysctl", &["-e", "-p", RESILIENCE_SYSCTL_FILE]);
    }
    // Persistent journal: ensure the on-disk dir, write the drop-in, and restart
    // journald so it migrates to disk this boot (not only next boot).
    let _ = std::fs::create_dir_all(JOURNAL_DIR);
    if let Some(parent) = Path::new(JOURNALD_PERSIST_CONF).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::write(JOURNALD_PERSIST_CONF, journald_persist_body()).is_ok() {
        set_mode(Path::new(JOURNALD_PERSIST_CONF), 0o644);
        let _ = exec::run("systemctl", &["restart", "systemd-journald"]);
        let _ = exec::run("journalctl", &["--flush"]);
    }

    // ── 7. Retention for the agent's own append-only files on disk. The journal
    // above is capped by journald itself; these are not capped by anything.
    if let Some(parent) = Path::new(LOG_RETENTION_TMPFILES).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::write(LOG_RETENTION_TMPFILES, log_retention_tmpfiles_body()).is_ok() {
        set_mode(Path::new(LOG_RETENTION_TMPFILES), 0o644);
        // Create the directories now so the rule owns them from this boot; the
        // daily clean timer applies the ages from here on.
        let _ = exec::run("systemd-tmpfiles", &["--create", LOG_RETENTION_TMPFILES]);
    }

    tracing::info!(
        "power hardening applied (WiFi/USB/EEE power-save off, freeze resilience armed, \
         on-disk log retention set)"
    );
}

/// Quiet the Rockchip BSP ISP 3A daemon (`rkaiq_3A.service`) on UVC-camera rigs.
/// Self-gating: a no-op on non-Rockchip boards (unit absent) and on boards where
/// rkaiq is doing real work (active). Ports `mask_unused_rockchip_isp_service`.
pub(super) fn mask_unused_rockchip_isp_service() {
    // Unit absent → nothing to do (non-Rockchip board).
    if !exec::run_ok("systemctl", &["list-unit-files", "rkaiq_3A.service"]) {
        return;
    }
    // Active → a real MIPI camera is using it; leave it alone.
    if exec::run_ok("systemctl", &["is-active", "--quiet", "rkaiq_3A.service"]) {
        return;
    }
    let _ = exec::run("systemctl", &["reset-failed", "rkaiq_3A.service"]);
    let _ = exec::run("systemctl", &["mask", "rkaiq_3A.service"]);
}

/// Install the SSH login banner from `data/motd/30-ados`. Linux-only (the bash
/// gates on `uname -s = Linux`). Ports 12-output.sh `install_motd`.
pub(super) fn install_motd(source: Option<&Path>) {
    if std::env::consts::OS != "linux" {
        return;
    }
    let src = match source.map(|s| s.join("data/motd/30-ados")) {
        Some(p) if p.is_file() => p,
        _ => {
            tracing::warn!("MOTD source not found; skipping login banner install");
            return;
        }
    };
    let _ = std::fs::create_dir_all("/etc/update-motd.d");
    if std::fs::copy(&src, MOTD_FILE).is_ok() {
        set_mode(Path::new(MOTD_FILE), 0o755);
        tracing::info!(path = MOTD_FILE, "SSH login banner installed");
    } else {
        tracing::warn!("copying MOTD banner failed");
    }
}

/// Install the static avahi service file (GROUND-STATION only) so the AP-side
/// `_ados._tcp` record is browseable even while the agent process is restarting.
/// The agent also registers the same service in-process with live TXT records;
/// this static copy is a fallback baseline. Best-effort, then reload avahi so the
/// new service is picked up without a full restart.
pub(super) fn install_avahi_gs_ap(source: Option<&Path>) {
    let src = match source.map(|s| s.join("data/avahi/ados-gs-ap.service")) {
        Some(p) if p.is_file() => p,
        _ => {
            tracing::warn!("avahi service source not found; skipping ados-gs-ap.service install");
            return;
        }
    };
    if let Some(parent) = Path::new(AVAHI_GS_AP_FILE).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::copy(&src, AVAHI_GS_AP_FILE).is_err() {
        tracing::warn!("copying avahi service file failed");
        return;
    }
    set_mode(Path::new(AVAHI_GS_AP_FILE), 0o644);
    // Reload avahi so the new service file is picked up without a full restart.
    let _ = exec::run("systemctl", &["reload", "avahi-daemon"]);
    tracing::info!(path = AVAHI_GS_AP_FILE, "avahi service file installed");
}
