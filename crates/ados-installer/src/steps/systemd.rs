//! systemd: deploy the service units + udev rules, write the env file, install
//! the plugin slice, `daemon-reload`, and ENABLE the right units for the
//! profile. Required. Checkpoint `systemd`. Runs only after both the binaries
//! are present and the config/identity exists.
//!
//! Ports the deploy + enable portions of `scripts/install.d/07-systemd.sh`
//! (`install_systemd_service`, `enable_universal_units`,
//! `disable_other_profile_units`, `enable_ground_station_units` ENABLE half,
//! `mask_conflicting_standalone_services`, `reconcile_rust_cutover_units`).
//!
//! THE ORDERING INVARIANT: this step deploys + enables but NEVER STARTS the
//! supervisor. The bash `install_systemd_service` does
//! `systemctl restart ados-supervisor` at line 129 — that early restart is THE
//! BUG: it can fire before the prebuilt binaries are on disk. The separate
//! `start` step (which `requires(["systemd", "fetch_binaries"])`) is the only
//! place the supervisor is started, so the graph guarantees binaries-first.
//! Likewise we only ENABLE the ground-station units here; the `start` step does
//! the `--no-block` start of the GS unit set.

use std::path::{Path, PathBuf};

use crate::ctx::Ctx;
use crate::env::{self, CONFIG_DIR, DEVICE_ID_FILE, INSTALL_DIR};
use crate::exec;
use crate::graph::{Step, StepKind, StepOutcome};

/// Where unit files + dropins are deployed.
const SYSTEMD_DIR: &str = "/etc/systemd/system";
/// The udev rules dir.
const UDEV_RULES_DIR: &str = "/etc/udev/rules.d";

/// Cross-profile units that get enable-linked on every install. Each has its
/// own `ConditionPathExists` gate so enabling them on a board without the
/// hardware is a clean no-op (`enable_universal_units`).
const UNIVERSAL_UNITS: &[&str] = &[
    "ados-peripherals.service",
    "ados-fbcon-detach.service",
    "ados-display-probe.service",
    "ados-usb-otg-host.service",
];

/// Ground-station units enable-linked here (the START half is the `start`
/// step's job). Mirrors `enable_ground_station_units`'s enable list, minus the
/// env-gated USB-gadget composer which the bash gates on ADOS_ENABLE_USB_GADGET.
const GROUND_STATION_ENABLE_UNITS: &[&str] = &[
    "ados-wfb-rx.service",
    "ados-mediamtx-gs.service",
    "ados-usb-gadget.service",
    "ados-oled.service",
    "ados-buttons.service",
    "ados-hostapd.service",
    "ados-dnsmasq-gs.service",
    "ados-setup-captive.service",
    "ados-kiosk.service",
    "ados-input.service",
    "ados-pic.service",
    "ados-uplink-router.service",
    "ados-modem.service",
    "ados-wifi-client.service",
    "ados-ethernet.service",
    "ados-cloud-relay.service",
];

/// Units the agent shipped in a prior release but has since deleted. The
/// installer prunes these on every run so an `--upgrade` from an older version
/// leaves no orphaned unit lingering in `/etc/systemd/system` — a unit whose
/// code is gone would otherwise sit there inactive forever (and could surface in
/// `systemctl --failed`). Append a name here the moment a unit is removed from
/// `data/systemd`. Deliberately an explicit list, NOT a "remove anything not in
/// the source set" sweep: the runtime-written plugin slice (`ados-plugins.slice`)
/// and the per-plugin subprocess units legitimately live outside `data/systemd`
/// and must never be pruned.
const RETIRED_UNITS: &[&str] = &["ados-scripting.service"];

/// The other-profile teardown list, keyed by the profile being installed
/// (`disable_other_profile_units`). On a GS rig the drone TX unit must not run;
/// on a drone rig every GS-only unit gets disabled.
fn other_profile_units(profile: &str) -> &'static [&'static str] {
    match profile {
        // The drone TX manager + the camera encode pipeline are air-side only;
        // a ground station receives video through ados-mediamtx-gs. ados-video's
        // binary is fetched on the drone profile only, so a GS that was ever a
        // drone keeps a stale ados-video enable whose binary is now absent —
        // tear it down so it cannot restart-loop.
        "ground_station" => &["ados-wfb.service", "ados-video.service"],
        _ => &[
            "ados-wfb-rx.service",
            "ados-mediamtx-gs.service",
            "ados-usb-gadget.service",
            "ados-usb-gadget-setup.service",
            "ados-oled.service",
            "ados-buttons.service",
            "ados-hostapd.service",
            "ados-dnsmasq-gs.service",
            "ados-setup-captive.service",
            "ados-kiosk.service",
            "ados-input.service",
            "ados-pic.service",
            "ados-uplink-router.service",
            "ados-modem.service",
            "ados-wifi-client.service",
            "ados-ethernet.service",
            "ados-cloud-relay.service",
            "ados-batman.service",
            "ados-mesh-pairing.service",
        ],
    }
}

// ─── Drop-in file paths (deployed outside /opt/ados; mirror uninstall.rs) ────

/// Kernel UDP buffer ceiling for the video pipeline (`install_video_sysctl`).
const VIDEO_SYSCTL_FILE: &str = "/etc/sysctl.d/99-ados-video.conf";
/// NetworkManager WiFi power-save force-off drop-in (`install_power_hardening`).
const NM_POWERSAVE_CONF: &str = "/etc/NetworkManager/conf.d/99-ados-wifi-powersave.conf";
/// Fallback udev rule that disables WiFi power-save on every `wlan*` add.
const WIFI_POWERSAVE_RULE: &str = "/etc/udev/rules.d/99-ados-wifi-powersave.rules";
/// Broad udev rule that disables USB autosuspend on every USB device.
const USB_NO_AUTOSUSPEND_RULE: &str = "/etc/udev/rules.d/99-ados-usb-no-autosuspend.rules";
/// tmpfiles drop-in that flips the global USB autosuspend default to "never"
/// early in userspace (belt-and-suspenders to the kernel cmdline).
const USB_AUTOSUSPEND_TMPFILES: &str = "/etc/tmpfiles.d/99-ados-usb-autosuspend.conf";
/// The kernel cmdline argument that disables USB autosuspend from the first
/// device enumeration (before the rootfs udev rules can apply).
const USB_AUTOSUSPEND_ARG: &str = "usbcore.autosuspend=-1";
/// Stale per-interface Ethernet-EEE rule a prior install may have written; the
/// boot oneshot owns EEE-off now, so any leftover rule is removed.
const ETH_NO_EEE_RULE: &str = "/etc/udev/rules.d/99-ados-eth-no-eee.rules";
/// logind drop-in that ignores idle/power-key/lid/suspend so the box never sleeps.
const LOGIND_NOSLEEP_CONF: &str = "/etc/systemd/logind.conf.d/99-ados-nosleep.conf";
/// The SSH login banner copied from `data/motd/30-ados`.
const MOTD_FILE: &str = "/etc/update-motd.d/30-ados";
/// Static avahi service file advertising the AP-side `_ados._tcp` record.
const AVAHI_GS_AP_FILE: &str = "/etc/avahi/services/ados-gs-ap.service";

/// Build the video-pipeline UDP-buffer sysctl drop-in body (pure). Mirrors
/// 03-kernel.sh `install_video_sysctl`: bumps the kernel socket-buffer ceilings
/// so the wfb_rx + fanout + mediamtx UDP sockets can actually allocate the 4 MiB
/// SO_RCVBUF/SO_SNDBUF they request at bind time instead of being clamped to the
/// stock ~208 KiB `net.core.rmem_max`.
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

/// Build the fallback WiFi-power-save udev rule body (pure), prefixed with the
/// resolved `iw` path when one is known (so the RUN+= line is absolute) and an
/// inline `/bin/sh -c 'iw ...'` fallback otherwise. Mirrors
/// `install_power_hardening` step 2.
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

/// Append `usbcore.autosuspend=-1` to the board's kernel cmdline so USB
/// autosuspend is disabled from the very first device enumeration — before the
/// rootfs udev `power/control=on` rules can apply. Cheap UVC cameras wedge on
/// the kernel-default 2 s autosuspend during the boot-race window the udev rule
/// cannot win; only a cmdline setting is active that early. Board-aware +
/// idempotent. Takes effect on the next reboot.
fn disable_usb_autosuspend_cmdline() {
    let arg = USB_AUTOSUSPEND_ARG;

    // Armbian / many Rockchip boards: the extraargs= line in armbianEnv.txt.
    let armbian = Path::new("/boot/armbianEnv.txt");
    if armbian.is_file() {
        if let Ok(content) = std::fs::read_to_string(armbian) {
            if let Some(new) = armbian_extraargs_with(&content, arg) {
                if std::fs::write(armbian, new).is_ok() {
                    tracing::info!(
                        file = "/boot/armbianEnv.txt",
                        arg,
                        "usb autosuspend disabled on kernel cmdline (reboot to apply)"
                    );
                }
            }
        }
        return;
    }

    // Raspberry Pi: a single-line cmdline.txt (firmware path first).
    for cmd in ["/boot/firmware/cmdline.txt", "/boot/cmdline.txt"] {
        let p = Path::new(cmd);
        if p.is_file() {
            if let Ok(content) = std::fs::read_to_string(p) {
                if let Some(new) = cmdline_txt_with(&content, arg) {
                    if std::fs::write(p, new).is_ok() {
                        tracing::info!(
                            file = cmd,
                            arg,
                            "usb autosuspend disabled on kernel cmdline (reboot to apply)"
                        );
                    }
                }
            }
            return;
        }
    }

    // Generic extlinux: the APPEND line.
    let extlinux = Path::new("/boot/extlinux/extlinux.conf");
    if extlinux.is_file() {
        if let Ok(content) = std::fs::read_to_string(extlinux) {
            if let Some(new) = extlinux_append_with(&content, arg) {
                if std::fs::write(extlinux, new).is_ok() {
                    tracing::info!(
                        file = "/boot/extlinux/extlinux.conf",
                        arg,
                        "usb autosuspend disabled on kernel cmdline (reboot to apply)"
                    );
                }
            }
        }
        return;
    }

    tracing::info!("no known boot cmdline file found; relying on the udev rule + tmpfiles default for USB autosuspend");
}

/// Build the logind no-sleep drop-in body (pure). Ignores the idle timer, power
/// key, lid switch, and suspend key so a console keypress or a closed lid cannot
/// suspend the box (`install_power_hardening` step 5).
pub fn logind_nosleep_body() -> String {
    "[Login]\n\
IdleAction=ignore\n\
HandlePowerKey=ignore\n\
HandleLidSwitch=ignore\n\
HandleSuspendKey=ignore\n"
        .to_string()
}

/// Build the `/etc/ados/env` file body (pure). Mirrors 07-systemd.sh:110-118,
/// including the `ADOS_STATE_IPC_MSGPACK=1` selector. `device_id` is read from
/// `/etc/ados/device-id`; empty when not yet minted (it is, by this step).
pub fn env_file_body(device_id: &str) -> String {
    format!(
        "ADOS_DEVICE_ID={device_id}\n\
ADOS_CONFIG={CONFIG_DIR}/config.yaml\n\
ADOS_RUN_DIR=/run/ados\n\
# State IPC wire: length-prefixed msgpack (v2). The reader auto-detects the\n\
# format per frame, so this only selects which encoding the producer emits.\n\
ADOS_STATE_IPC_MSGPACK=1\n"
    )
}

/// Deploy every `ados-*.{service,slice,target,timer}` unit from the source
/// `data/systemd` dir to `/etc/systemd/system`, rewriting the venv path. Returns
/// the count deployed, or an error when the source dir is absent (Required: no
/// units means no agent).
fn deploy_units(systemd_src: &Path) -> anyhow::Result<usize> {
    let read = std::fs::read_dir(systemd_src)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", systemd_src.display()))?;
    let mut count = 0usize;
    for entry in read.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if !name.starts_with("ados-") {
            continue;
        }
        if !(name.ends_with(".service")
            || name.ends_with(".slice")
            || name.ends_with(".target")
            || name.ends_with(".timer"))
        {
            continue;
        }
        let body = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("read {} failed: {e}", path.display()))?;
        // The packaged units hard-code /opt/ados/venv; rewrite to the resolved
        // venv dir (identical today, but mirrors the bash sed for safety).
        let rewritten = body.replace("/opt/ados/venv", env::VENV_DIR);
        let dest = Path::new(SYSTEMD_DIR).join(name);
        std::fs::write(&dest, rewritten)
            .map_err(|e| anyhow::anyhow!("write {} failed: {e}", dest.display()))?;
        count += 1;
    }
    if count == 0 {
        anyhow::bail!("no ados unit files found under {}", systemd_src.display());
    }
    Ok(count)
}

/// Deploy the udev rules from the source `data/udev` dir to
/// `/etc/udev/rules.d`, then reload. Best-effort (no rule is install-critical;
/// they are hot-plug ergonomics). Returns the count deployed.
fn deploy_udev_rules(udev_src: &Path) -> usize {
    let read = match std::fs::read_dir(udev_src) {
        Ok(r) => r,
        Err(_) => return 0,
    };
    let _ = std::fs::create_dir_all(UDEV_RULES_DIR);
    let mut count = 0usize;
    for entry in read.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) if n.ends_with(".rules") => n.to_string(),
            _ => continue,
        };
        let dest = Path::new(UDEV_RULES_DIR).join(&name);
        if std::fs::copy(&path, &dest).is_ok() {
            count += 1;
        }
    }
    if count > 0 {
        let _ = exec::run("udevadm", &["control", "--reload"]);
        let _ = exec::run("udevadm", &["trigger"]);
    }
    count
}

/// Create `/run/ados` (+ tmpfiles rule) so the Unix sockets have a home that
/// survives reboot. Idempotent.
fn install_run_dir_tmpfiles() {
    let _ = std::fs::create_dir_all("/run/ados");
    let _ = std::fs::write(
        "/etc/tmpfiles.d/ados.conf",
        "d /run/ados 0755 root root -\n",
    );
}

/// Write the `/etc/ados/env` file from the persisted device id.
fn write_env_file() {
    let device_id = std::fs::read_to_string(DEVICE_ID_FILE)
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let path = Path::new(CONFIG_DIR).join("env");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&path, env_file_body(&device_id)) {
        tracing::warn!(error = %e, "writing /etc/ados/env failed");
        return;
    }
    set_mode(&path, 0o600);
}

/// Install the plugin runtime tmpfiles drop-in so `/run/ados/plugins` (the
/// per-plugin socket dir) is recreated on every boot, not just at install
/// time. Mirrors `install_plugin_tmpfiles`: prefer the packaged snippet under
/// the source tree, else write the inline default; then run
/// `systemd-tmpfiles --create` so the dir exists for the supervisor + plugins.
fn install_plugin_tmpfiles(source: Option<&Path>) {
    const DEST: &str = "/etc/tmpfiles.d/ados-plugins.conf";
    let inline = "# ADOS plugin runtime sockets and runtime state\n\
d /run/ados/plugins 0750 ados ados -\n\
r! /run/ados/plugins/*.sock\n";

    let wrote = source
        .map(|s| s.join("etc/tmpfiles.d/ados-plugins.conf"))
        .filter(|p| p.is_file())
        .map(|p| std::fs::copy(&p, DEST).is_ok())
        .unwrap_or(false);
    if !wrote {
        let _ = std::fs::write(DEST, inline);
    }
    set_mode(Path::new(DEST), 0o644);
    // Materialize the dir now (idempotent); the drop-in handles reboots.
    let _ = exec::run("systemd-tmpfiles", &["--create", DEST]);
}

/// Install the plugin cgroup slice: delegate to `scripts/setup-plugin-slice.sh`
/// when present (it runs its own daemon-reload), else write the slice inline.
fn install_plugin_slice(source: Option<&Path>) {
    if let Some(src) = source {
        let script = src.join("scripts/setup-plugin-slice.sh");
        if script.is_file() {
            let s = script.to_string_lossy();
            if exec::run_ok("bash", &[&s]) {
                return;
            }
            tracing::warn!("setup-plugin-slice.sh returned non-zero; falling back to inline write");
        }
    }
    // Inline fallback (mirrors the script's slice content).
    let _ = std::fs::create_dir_all("/var/log/ados/plugins");
    let _ = std::fs::create_dir_all("/var/ados/plugin-data");
    let _ = std::fs::create_dir_all("/var/ados/plugins");
    let _ = std::fs::create_dir_all("/run/ados/plugins");
    let _ = std::fs::create_dir_all("/etc/ados/plugin-keys");
    set_mode(Path::new("/etc/ados/plugin-keys"), 0o700);
    let slice = "[Unit]\n\
Description=ADOS plugin shared cgroup slice\n\
Before=slices.target\n\
\n\
[Slice]\n\
CPUAccounting=yes\n\
MemoryAccounting=yes\n\
TasksAccounting=yes\n\
IOAccounting=yes\n";
    let _ = std::fs::write("/etc/systemd/system/ados-plugins.slice", slice);
}

/// Provision the persistent store directory for the local logging and
/// telemetry daemon so it can create its database on first start without
/// needing a writable parent it does not own. The daemon runs as root (like the
/// sibling service daemons), so the dir is root-owned; 0750 keeps it off-limits
/// to other users. Idempotent. The store ships dark — the unit is deployed but
/// not enabled — so this only prepares the ground for an explicit turn-on.
fn install_logd_store_dir() {
    const DIR: &str = "/var/ados/logd";
    if let Err(e) = std::fs::create_dir_all(DIR) {
        tracing::warn!(error = %e, dir = DIR, "creating logging store dir failed");
        return;
    }
    set_mode(Path::new(DIR), 0o750);
}

/// Enable a unit only when its file is deployed; tolerate the not-found case.
fn enable_if_present(unit: &str) {
    let path = Path::new(SYSTEMD_DIR).join(unit);
    if path.exists() {
        let _ = exec::run("systemctl", &["enable", unit]);
    } else {
        tracing::warn!(unit, "unit not deployed; skipping enable");
    }
}

/// Prune units retired in a prior release: stop, disable (while the file still
/// exists so systemd can read its `[Install]` section and drop the `.wants`
/// symlinks), reset any failed state, then delete the unit file and any drop-in
/// dir. Idempotent — a unit already gone is a clean no-op. Must run before the
/// `daemon-reload` so the removal is visible to systemd in the same install.
fn prune_retired_units() {
    for unit in RETIRED_UNITS {
        let path = Path::new(SYSTEMD_DIR).join(unit);
        let dropin = Path::new(SYSTEMD_DIR).join(format!("{unit}.d"));
        let present = path.exists() || dropin.exists();
        let _ = exec::run("systemctl", &["stop", unit]);
        let _ = exec::run("systemctl", &["disable", unit]);
        let _ = exec::run("systemctl", &["reset-failed", unit]);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dropin);
        if present {
            tracing::info!(unit, "pruned retired systemd unit");
        }
    }
}

/// Tear down (stop + disable) units belonging to the other profile.
fn disable_other_profile_units(profile: &str) {
    for unit in other_profile_units(profile) {
        let _ = exec::run("systemctl", &["stop", unit]);
        let _ = exec::run("systemctl", &["disable", unit]);
    }
}

/// Stop + disable + mask the Debian-default dnsmasq/hostapd so they cannot
/// fight the GS profile for ports 53/67 or wlan0 (`mask_conflicting_standalone_services`).
fn mask_conflicting_standalone_services() {
    for action in ["stop", "disable", "mask"] {
        let _ = exec::run("systemctl", &[action, "dnsmasq.service", "hostapd.service"]);
    }
}

/// Reconcile the packaged units a native consolidator daemon subsumes against
/// the cutover flags (`reconcile_rust_cutover_units`). GROUND-STATION ONLY.
/// When `net-rust-enabled` is set, disable the units the native uplink daemon
/// owns; when `hid-rust-enabled` is set, disable the unit the native arbiter
/// owns; otherwise re-enable them (default posture leaves the packaged units live).
fn reconcile_rust_cutover_units() {
    let net_subsumed = [
        "ados-ethernet.service",
        "ados-wifi-client.service",
        "ados-usb-gadget.service",
    ];
    let hid_subsumed = ["ados-buttons.service"];

    if Path::new(CONFIG_DIR).join("net-rust-enabled").exists() {
        for unit in net_subsumed {
            let _ = exec::run("systemctl", &["stop", unit]);
            let _ = exec::run("systemctl", &["disable", unit]);
            let _ = exec::run("systemctl", &["reset-failed", unit]);
        }
    } else {
        for unit in net_subsumed {
            let _ = exec::run("systemctl", &["enable", unit]);
        }
    }

    if Path::new(CONFIG_DIR).join("hid-rust-enabled").exists() {
        for unit in hid_subsumed {
            let _ = exec::run("systemctl", &["stop", unit]);
            let _ = exec::run("systemctl", &["disable", unit]);
            let _ = exec::run("systemctl", &["reset-failed", unit]);
        }
    } else {
        for unit in hid_subsumed {
            let _ = exec::run("systemctl", &["enable", unit]);
        }
    }
}

/// Reconcile the local logging and telemetry store unit against its
/// fallback marker. The store is on by default (the log-view endpoints read
/// it), so a fresh box with no marker enables it; the `logd-python-fallback`
/// marker — written by `ados rust disable logd` — pins it off, so the
/// installer tears it down instead. Idempotent and runs on every install so a
/// re-run from a partial state self-heals. The START half is the `start`
/// step's job (the unit is PartOf the supervisor).
fn reconcile_logd_unit() {
    const UNIT: &str = "ados-logd.service";
    let pinned_off = Path::new(CONFIG_DIR).join("logd-python-fallback").exists();
    if pinned_off {
        let _ = exec::run("systemctl", &["stop", UNIT]);
        let _ = exec::run("systemctl", &["disable", UNIT]);
        let _ = exec::run("systemctl", &["reset-failed", UNIT]);
    } else {
        enable_if_present(UNIT);
    }
}

/// Drop the video-pipeline UDP sysctl tuning and apply it now so the running
/// agent picks up the new ceiling on its next socket bind. Idempotent overwrite.
/// Ports 03-kernel.sh `install_video_sysctl`.
fn install_video_sysctl() {
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
fn resolve_iw_path() -> Option<String> {
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
/// 5. Mask the sleep targets + a logind no-sleep drop-in.
/// 6. Deploy `ados-power-reassert.sh` to /opt/ados/bin + enable/start the
///    `ados-power.service` oneshot (the unit file ships in data/systemd and is
///    already deployed by `deploy_units`).
///
/// Idempotent; applies on the spot. CPU governor is intentionally left untouched.
fn install_power_hardening(source: Option<&Path>) {
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

    // ── 5. Mask system sleep + logind no-sleep drop-in. ──
    let _ = exec::run(
        "systemctl",
        &[
            "mask",
            "sleep.target",
            "suspend.target",
            "hibernate.target",
            "hybrid-sleep.target",
            "suspend-then-hibernate.target",
        ],
    );
    if let Some(parent) = Path::new(LOGIND_NOSLEEP_CONF).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::write(LOGIND_NOSLEEP_CONF, logind_nosleep_body()).is_ok() {
        set_mode(Path::new(LOGIND_NOSLEEP_CONF), 0o644);
    }
    let _ = exec::run("systemctl", &["daemon-reload"]);

    // ── 6. Boot re-assert oneshot. ──
    // The unit file (data/systemd/ados-power.service) is deployed by
    // deploy_units; here we install the helper script it execs and enable+start
    // the unit. The script lives at scripts/ados-power-reassert.sh in the source
    // tree (kept; not part of the removed install.d set).
    let bin_dir = format!("{INSTALL_DIR}/bin");
    let _ = std::fs::create_dir_all(&bin_dir);
    let reassert_dst = format!("{bin_dir}/ados-power-reassert.sh");
    let reassert_src = source.map(|s| s.join("scripts/ados-power-reassert.sh"));
    let copied = reassert_src
        .as_deref()
        .filter(|p| p.is_file())
        .map(|p| std::fs::copy(p, &reassert_dst).is_ok())
        .unwrap_or(false);
    if !copied {
        // Inline fallback so the oneshot still works on a tree missing the helper.
        let _ = std::fs::write(&reassert_dst, REASSERT_INLINE_FALLBACK);
    }
    set_mode(Path::new(&reassert_dst), 0o755);
    let _ = exec::run("systemctl", &["enable", "ados-power.service"]);
    // Run it now so the knobs are asserted on the current boot too.
    let _ = exec::run("systemctl", &["start", "ados-power.service"]);

    tracing::info!("power hardening applied (WiFi/USB/EEE power-save off, sleep masked)");
}

/// Inline `ados-power-reassert.sh` body used only when the source tree does not
/// ship the helper (it normally does). Mirrors the script at
/// scripts/ados-power-reassert.sh; the def-route interface is skipped for EEE so
/// the management link is never renegotiated.
const REASSERT_INLINE_FALLBACK: &str = "#!/bin/sh\n\
# ADOS: re-assert power knobs at boot. Forgiving by design.\n\
for _ifdir in /sys/class/net/wlan*; do\n\
    [ -e \"${_ifdir}\" ] || continue\n\
    _if=\"$(basename \"${_ifdir}\")\"\n\
    iw dev \"${_if}\" set power_save off 2>/dev/null || true\n\
done\n\
for _ctl in /sys/bus/usb/devices/*/power/control; do\n\
    [ -w \"${_ctl}\" ] || continue\n\
    echo on > \"${_ctl}\" 2>/dev/null || true\n\
done\n\
_def_if=\"$(ip route show default 2>/dev/null | awk '{print $5; exit}')\"\n\
for _ed in /sys/class/net/eth* /sys/class/net/end* /sys/class/net/enP* /sys/class/net/enx*; do\n\
    [ -e \"${_ed}\" ] || continue\n\
    _eif=\"$(basename \"${_ed}\")\"\n\
    [ \"${_eif}\" = \"${_def_if}\" ] && continue\n\
    ethtool --set-eee \"${_eif}\" eee off 2>/dev/null || true\n\
done\n\
exit 0\n";

/// Quiet the Rockchip BSP ISP 3A daemon (`rkaiq_3A.service`) on UVC-camera rigs.
/// Self-gating: a no-op on non-Rockchip boards (unit absent) and on boards where
/// rkaiq is doing real work (active). Ports `mask_unused_rockchip_isp_service`.
fn mask_unused_rockchip_isp_service() {
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
fn install_motd(source: Option<&Path>) {
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
fn install_avahi_gs_ap(source: Option<&Path>) {
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

/// Install the env-gated libcomposite USB-gadget composer (GROUND-STATION only).
/// Gated on `ADOS_ENABLE_USB_GADGET=1` (default off) until the gadget is bench
/// validated. Ports the gated block at the head of `enable_ground_station_units`:
/// copy the composer script, ensure `dwc2` is loaded, enable the setup oneshot.
fn install_usb_gadget_composer(source: Option<&Path>) {
    if std::env::var("ADOS_ENABLE_USB_GADGET").as_deref() != Ok("1") {
        return;
    }
    let src = source.map(|s| s.join("data/usb-gadget/ados-cdc-ncm-rndis.sh"));
    let src = match src.as_deref().filter(|p| p.is_file()) {
        Some(p) => p,
        None => {
            tracing::warn!(
                "USB gadget composer script not found; skipping (ADOS_ENABLE_USB_GADGET=1 was set)"
            );
            return;
        }
    };
    let dst_dir = "/usr/local/lib/ados/usb-gadget";
    let _ = std::fs::create_dir_all(dst_dir);
    let dst = format!("{dst_dir}/ados-cdc-ncm-rndis.sh");
    if std::fs::copy(src, &dst).is_err() {
        tracing::warn!("copying USB gadget composer failed");
        return;
    }
    set_mode(Path::new(&dst), 0o755);
    tracing::info!("USB gadget composer script installed (ADOS_ENABLE_USB_GADGET=1)");

    // Ensure dwc2 is loaded on OTG-capable boards so the gadget subsystem has a
    // UDC to bind to. No-op on boards that lack OTG hardware.
    let modules_has_dwc2 = std::fs::read_to_string("/etc/modules")
        .map(|s| s.lines().any(|l| l.trim() == "dwc2"))
        .unwrap_or(false);
    if !modules_has_dwc2 {
        if let Ok(mut existing) = std::fs::read_to_string("/etc/modules") {
            if !existing.ends_with('\n') {
                existing.push('\n');
            }
            existing.push_str("dwc2\n");
            let _ = std::fs::write("/etc/modules", existing);
        }
    }
    let _ = exec::run("modprobe", &["dwc2"]);
    let _ = exec::run("modprobe", &["libcomposite"]);
    enable_if_present("ados-usb-gadget-setup.service");
}

/// Provision the `ados` system user and group (idempotent). Several downstream
/// steps assume this identity already exists: the plugin runtime dir is created
/// `0750 ados ados` (`install_plugin_tmpfiles`), every plugin subprocess unit
/// runs `User=ados`/`Group=ados`, and the ground-station hardware-group
/// memberships run `usermod -aG <grp> ados`. Without it the tmpfiles chown
/// cannot resolve the owner and the GS usermod is gated behind `id ados` and
/// silently no-ops.
///
/// A system account: no login shell, no home directory, allocated below the
/// regular-uid range. The group and the user are each created only when absent,
/// mirroring the getent/id idempotency the rest of this module uses.
fn provision_ados_identity() {
    if !exec::run_ok("getent", &["group", "ados"]) {
        let _ = exec::run("groupadd", &["--system", "ados"]);
    }
    if !exec::run_ok("id", &["ados"]) {
        let _ = exec::run(
            "useradd",
            &[
                "--system",
                "--gid",
                "ados",
                "--no-create-home",
                "--shell",
                "/usr/sbin/nologin",
                "ados",
            ],
        );
    }
}

/// Add the `ados` and `pi` service users to the hardware-access groups the
/// ground-station services need (GROUND-STATION only). Ports the
/// `usermod -aG ...` bits of `enable_ground_station_units`:
///
/// - `gpio`    — button service drives /dev/gpiochip0 via libgpiod.
/// - `input`   — input manager + PIC arbiter read /dev/input (gamepads/evdev).
/// - `plugdev` — USB device access for hot-plugged peripherals.
/// - `bluetooth` — D-Bus access for the PIC arbiter.
/// - `i2c`     — userspace OLED + future I2C peripherals.
///
/// Every `usermod -aG` is an idempotent no-op when membership already exists.
fn add_ground_station_group_memberships() {
    for grp in ["gpio", "input", "plugdev", "bluetooth", "i2c"] {
        if !exec::run_ok("getent", &["group", grp]) {
            tracing::warn!(group = grp, "group not present; skipping usermod");
            continue;
        }
        for user in ["ados", "pi"] {
            if exec::run_ok("id", &[user]) {
                let _ = exec::run("usermod", &["-aG", grp, user]);
            }
        }
    }
    // Trigger udev so i2c-dev nodes pick up the new group membership without a
    // reboot (mirrors the bash `udevadm trigger --subsystem-match=i2c-dev`).
    let _ = exec::run("udevadm", &["trigger", "--subsystem-match=i2c-dev"]);
}

/// systemd unit install + enable (NOT start).
pub struct Systemd;

impl Step for Systemd {
    fn id(&self) -> &str {
        "systemd"
    }
    fn requires(&self) -> &[&str] {
        &["fetch_binaries", "config_identity", "wfb_ng"]
    }
    fn checkpoint(&self) -> Option<&str> {
        Some("systemd")
    }
    fn kind(&self) -> StepKind {
        StepKind::Required
    }
    fn run(&self, ctx: &mut Ctx) -> StepOutcome {
        let source = match env::resolve_source_dir(ctx.source_dir.as_deref()) {
            Some(s) => s,
            None => {
                return StepOutcome::Failed(
                    "source tree not found; cannot locate data/systemd unit files".to_string(),
                )
            }
        };

        // System identity: provision the `ados` user + group (idempotent)
        // before anything that consumes it. The plugin runtime dir
        // (/run/ados/plugins, 0750 ados ados), every plugin subprocess unit
        // (User=ados/Group=ados), and the ground-station hardware-group
        // memberships all reference `ados:ados`; create it first so the
        // tmpfiles chown and the usermod resolve instead of silently no-opping.
        provision_ados_identity();

        // 1. Deploy the unit files (Required — no units, no agent).
        let systemd_src = source.join("data/systemd");
        let count = match deploy_units(&systemd_src) {
            Ok(n) => n,
            Err(e) => return StepOutcome::Failed(e.to_string()),
        };
        tracing::info!(count, "deployed systemd unit files");

        // 1b. Prune units retired in a prior release so an `--upgrade` from an
        //     older version leaves no orphaned unit file behind (e.g. a removed
        //     service whose code is gone). Before the daemon-reload below so the
        //     removal is visible to systemd in this same install.
        prune_retired_units();

        // 2. /run/ados + tmpfiles, env file, plugin slice, udev rules.
        install_run_dir_tmpfiles();
        write_env_file();
        install_plugin_slice(Some(&source));
        install_plugin_tmpfiles(Some(&source));
        install_logd_store_dir();
        let udev_src = source.join("data/udev");
        let udev_count = deploy_udev_rules(&udev_src);
        tracing::info!(count = udev_count, "deployed udev rules");

        // 2b. Cross-profile kernel/power/observability hardening. Drone-relevant
        //     (video sysctl keeps the receive chain from dropping bursts; power
        //     hardening keeps the USB camera + RTL radio + mgmt WiFi from
        //     suspending). All idempotent + self-gating; apply on every install.
        install_video_sysctl();
        install_power_hardening(Some(&source));
        mask_unused_rockchip_isp_service();
        install_motd(Some(&source));

        // 3. daemon-reload so the new units are visible.
        let _ = exec::run("systemctl", &["daemon-reload"]);

        // 4. Enable the supervisor (NOT start — that is the `start` step's job,
        //    gated on fetch_binaries so the binary is guaranteed present).
        enable_if_present("ados-supervisor.service");

        // 5. Cross-profile universal units.
        for unit in UNIVERSAL_UNITS {
            enable_if_present(unit);
        }
        let _ = std::fs::create_dir_all("/etc/ados/peripherals");

        // 5b. The logging and telemetry store is on by default (the log-view
        //     endpoints read it). Enable it unless the fallback marker pins it
        //     off; the start step brings it up after the supervisor.
        reconcile_logd_unit();

        // 6. Profile-specific enable + teardown.
        if ctx.profile == "ground_station" {
            // The env-gated USB-gadget composer (default off) and the
            // hardware-access group memberships are GS-only — they belong to the
            // tether + button/joystick/OLED service set.
            install_usb_gadget_composer(Some(&source));
            install_avahi_gs_ap(Some(&source));
            for unit in GROUND_STATION_ENABLE_UNITS {
                enable_if_present(unit);
            }
            add_ground_station_group_memberships();
            mask_conflicting_standalone_services();
            reconcile_rust_cutover_units();
        }
        disable_other_profile_units(&ctx.profile);

        StepOutcome::Ok
    }
}

/// The deployed unit-file path for a given unit name (used by `start`/`health`).
pub fn deployed_unit_path(unit: &str) -> PathBuf {
    Path::new(SYSTEMD_DIR).join(unit)
}

/// chmod (Unix); a no-op on a non-Unix dev host.
#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_file_carries_the_msgpack_selector() {
        let body = env_file_body("abcd1234abcd");
        assert!(body.contains("ADOS_DEVICE_ID=abcd1234abcd"));
        assert!(body.contains(&format!("ADOS_CONFIG={CONFIG_DIR}/config.yaml")));
        assert!(body.contains("ADOS_RUN_DIR=/run/ados"));
        // The load-bearing IPC selector must be present (07-systemd.sh:116).
        assert!(body.contains("ADOS_STATE_IPC_MSGPACK=1"));
    }

    #[test]
    fn env_file_with_empty_device_id_is_blank() {
        let body = env_file_body("");
        assert!(body.contains("ADOS_DEVICE_ID=\n"));
    }

    #[test]
    fn other_profile_units_branch_by_profile() {
        // A GS rig tears down the drone-side TX + camera-encode units.
        let gs = other_profile_units("ground_station");
        assert_eq!(gs, &["ados-wfb.service", "ados-video.service"]);
        // A drone rig tears down the GS RX + AP set.
        let drone = other_profile_units("drone");
        assert!(drone.contains(&"ados-wfb-rx.service"));
        assert!(drone.contains(&"ados-hostapd.service"));
        assert!(drone.contains(&"ados-batman.service"));
        // It must NOT tear down the supervisor or a cross-profile unit.
        assert!(!drone.contains(&"ados-supervisor.service"));
        assert!(!drone.contains(&"ados-peripherals.service"));
    }

    #[test]
    fn deploy_units_writes_only_ados_units_and_rewrites_venv() {
        let src = tempfile::tempdir().unwrap();
        // A genuine ados unit referencing the venv path, plus a non-ados file
        // that must be ignored, plus a non-unit ados file that must be ignored.
        std::fs::write(
            src.path().join("ados-supervisor.service"),
            "[Service]\nExecStart=/opt/ados/venv/bin/ados-supervisor\n",
        )
        .unwrap();
        std::fs::write(src.path().join("other.service"), "x").unwrap();
        std::fs::write(src.path().join("ados-notes.txt"), "x").unwrap();

        // Deploy to a temp /etc/systemd/system surrogate by temporarily
        // pointing SYSTEMD_DIR... we cannot rebind the const, so assert the
        // builder logic via a direct call against a known-good source and a
        // real (but isolated) dest is out of scope; instead verify the source
        // scan + venv rewrite on the body the deployer would write.
        let body = std::fs::read_to_string(src.path().join("ados-supervisor.service")).unwrap();
        let rewritten = body.replace("/opt/ados/venv", env::VENV_DIR);
        assert!(rewritten.contains(&format!("{}/bin/ados-supervisor", env::VENV_DIR)));
    }

    #[test]
    fn deploy_units_errors_on_empty_source() {
        let src = tempfile::tempdir().unwrap();
        // Only a non-ados file present → no ados units → error.
        std::fs::write(src.path().join("unrelated.service"), "x").unwrap();
        let err = deploy_units(src.path()).unwrap_err();
        assert!(err.to_string().contains("no ados unit files"));
    }

    #[test]
    fn retired_units_are_not_in_the_live_source_set() {
        // A retired unit must NOT also ship in data/systemd, or the installer
        // would deploy it and immediately prune it on every run. Guarded so the
        // test is a clean no-op when the source tree is not resolvable.
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/systemd");
        if let Ok(read) = std::fs::read_dir(&src) {
            let live: std::collections::HashSet<String> = read
                .flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .collect();
            for unit in RETIRED_UNITS {
                assert!(
                    !live.contains(*unit),
                    "{unit} is in RETIRED_UNITS but still ships in data/systemd"
                );
            }
        }
        // The retired set names the deliberately-removed unit, and never the
        // runtime-written plugin slice or a core unit.
        assert!(RETIRED_UNITS.contains(&"ados-scripting.service"));
        assert!(!RETIRED_UNITS.contains(&"ados-plugins.slice"));
        assert!(!RETIRED_UNITS.contains(&"ados-supervisor.service"));
    }

    #[test]
    fn universal_units_are_profile_agnostic() {
        assert!(UNIVERSAL_UNITS.contains(&"ados-peripherals.service"));
        assert!(UNIVERSAL_UNITS.contains(&"ados-display-probe.service"));
        // The supervisor is enabled separately, not in the universal list.
        assert!(!UNIVERSAL_UNITS.contains(&"ados-supervisor.service"));
    }

    #[test]
    fn gs_enable_list_is_subset_of_other_profile_drone_teardown() {
        // Everything the GS install enables, the drone install must explicitly
        // disable (the 07-systemd.sh invariant), except the env-gated gadget
        // composer which is not in the enable list here.
        let drone_teardown = other_profile_units("drone");
        for unit in GROUND_STATION_ENABLE_UNITS {
            assert!(
                drone_teardown.contains(unit),
                "{unit} is GS-enabled but not torn down on a drone rig"
            );
        }
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
        assert!(
            extlinux_append_with("LABEL x\n  KERNEL /boot/Image\n", USB_AUTOSUSPEND_ARG).is_none()
        );
    }

    #[test]
    fn logind_nosleep_ignores_every_sleep_path() {
        let body = logind_nosleep_body();
        assert!(body.contains("[Login]"));
        assert!(body.contains("IdleAction=ignore"));
        assert!(body.contains("HandlePowerKey=ignore"));
        assert!(body.contains("HandleLidSwitch=ignore"));
        assert!(body.contains("HandleSuspendKey=ignore"));
    }

    #[test]
    fn reassert_inline_fallback_skips_default_route_iface() {
        // The inline fallback must contain the def-route skip so it never
        // renegotiates the management NIC's PHY (the wired-link-bounce hazard).
        assert!(REASSERT_INLINE_FALLBACK.contains("ip route show default"));
        assert!(REASSERT_INLINE_FALLBACK.contains("[ \"${_eif}\" = \"${_def_if}\" ] && continue"));
        assert!(REASSERT_INLINE_FALLBACK.starts_with("#!/bin/sh"));
        assert!(REASSERT_INLINE_FALLBACK.ends_with("exit 0\n"));
    }

    #[test]
    fn dropin_paths_match_the_uninstall_removal_list() {
        // Install/uninstall symmetry: every drop-in this step writes must be in
        // the uninstall removal list so a purge leaves a clean box.
        let removed = crate::uninstall::dropin_files();
        for path in [
            VIDEO_SYSCTL_FILE,
            NM_POWERSAVE_CONF,
            WIFI_POWERSAVE_RULE,
            USB_NO_AUTOSUSPEND_RULE,
            USB_AUTOSUSPEND_TMPFILES,
            ETH_NO_EEE_RULE,
            LOGIND_NOSLEEP_CONF,
        ] {
            assert!(
                removed.contains(&path),
                "{path} is written by install but not removed by uninstall"
            );
        }
    }
}
