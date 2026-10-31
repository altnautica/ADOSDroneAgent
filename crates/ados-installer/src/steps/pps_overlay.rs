//! Enable a kernel PPS source on the GNSS receiver's PPS line when the board
//! profile declares one (`gnss.pps_gpio`), so `/dev/ppsN` exists for the GNSS
//! service and chrony.
//!
//! What it does, by SoC family:
//!   * Rockchip: renders a `pps-gpio` device-tree overlay ([`pps_overlay_dts`]),
//!     compiles it with `dtc -@`, installs it and enables it the way the image
//!     boots, mirroring the display installer's Rockchip activation
//!     (`scripts/drivers/install-display-overlay.sh`):
//!       - Radxa vendor images: `/boot/dtbo/ados-pps.dtbo`, registered in
//!         `/boot/dtbo/managed.list`, then `u-boot-update` regenerates
//!         `extlinux.conf` (snapshot, size check, restore on failure);
//!       - Armbian: `/boot/overlay-user/ados-pps.dtbo` named in the
//!         `user_overlays=` line of `/boot/armbianEnv.txt`.
//!   * Raspberry Pi: no generated source. The firmware ships a stock `pps-gpio`
//!     overlay, so the boot config gains `dtoverlay=pps-gpio,gpiopin=N`.
//!
//! Every boot-config edit goes through [`boot_config`], whose keep-first
//! snapshot preserves the pristine baseline the display probe restores from.
//!
//! Gated by [`gate::pps_gpio_for`]: only a ground station or visor whose board
//! GNSS receiver (serial or sensor hub) drives the PPS line. Fails safe: a
//! board with no `pps_gpio`, an unknown SoC family, an
//! out-of-range line, a missing `dtc` or an unrecognised boot mechanism leaves
//! the boot configuration untouched and logs why. Idempotent: a re-run with the
//! overlay already enabled changes nothing and asks for no reboot.
//!
//! Not yet validated on hardware: the generated overlay and both Rockchip
//! activation paths are untested on a real ROCK 5B+ or ROCK 5C.

use std::path::Path;

use ados_hal_probe::board_sidecar::{load_profiles, resolve_profile};
use ados_hal_probe::host_facts::probe_host_facts;

use crate::ctx::Ctx;
use crate::graph::{Step, StepKind, StepOutcome};
use crate::steps::boot_config::{self, BootConfigEdit};
use crate::steps::wfb_ng::on_path;
use ados_term::exec;

/// Basename of the generated overlay (`<name>.dts` / `<name>.dtbo`).
pub const OVERLAY_NAME: &str = "ados-pps";
const BOOT_DIR: &str = "/boot";
/// Shared with the display and camera installers: one file, merged, never
/// clobbered.
const MODULES_LOAD_FILE: &str = "/etc/modules-load.d/ados-hardware.conf";
const PPS_MODULE: &str = "pps-gpio";
/// RK3566/RK3568/RK3582/RK3588 expose GPIO banks 0..=4 (`gpio0`..`gpio4`).
const ROCKCHIP_GPIO_BANKS: i64 = 5;
/// The highest BCM line on the Pi 40-pin header.
const PI_MAX_GPIO: i64 = 27;
/// A regenerated `extlinux.conf` below this size is treated as truncated.
const EXTLINUX_MIN_BYTES: u64 = 100;

/// The SoC families this step knows how to provision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocFamily {
    Rockchip,
    Broadcom,
}

/// Classify a profile's `soc` string (pure).
pub fn soc_family(soc: &str) -> Option<SocFamily> {
    let s = soc.trim().to_ascii_uppercase();
    if s.starts_with("RK") {
        Some(SocFamily::Rockchip)
    } else if s.starts_with("BCM") {
        Some(SocFamily::Broadcom)
    } else {
        None
    }
}

/// Split a Rockchip Linux GPIO number into `(bank, line)`: bank = n / 32,
/// line = n % 32. `None` when the number is outside the five banks.
pub fn rockchip_bank_line(gpio: i64) -> Option<(i64, i64)> {
    if gpio < 0 || gpio / 32 >= ROCKCHIP_GPIO_BANKS {
        return None;
    }
    Some((gpio / 32, gpio % 32))
}

/// The Rockchip pin name for a Linux GPIO number (`100` -> `GPIO3_A4`).
fn rockchip_pin_name(bank: i64, line: i64) -> String {
    let group = ['A', 'B', 'C', 'D'][(line / 8) as usize];
    format!("GPIO{bank}_{group}{}", line % 8)
}

/// The `pps-gpio` overlay source for `gpio` on a Rockchip `soc` (pure).
///
/// The bank phandle labels are the vendor kernel's (`gpio0: gpio@fd8a0000` ...
/// in `rk3588s.dtsi`). The cell values are written as plain numbers so the
/// source compiles with `dtc` alone, without a cpp pass over the kernel's
/// dt-bindings headers: the line cell is the `RK_P<group><index>` value
/// (`line % 32`), and flags `0` is `GPIO_ACTIVE_HIGH`. No pinctrl entry: Radxa's
/// own GPIO-consumer overlays (`rk3588-w1-gpio*`) claim a header line the same
/// way, relying on the pin's reset GPIO function.
///
/// `None` for a non-Rockchip SoC (the Pi uses its stock overlay) or a line
/// outside the GPIO banks.
pub fn pps_overlay_dts(soc: &str, gpio: i64) -> Option<String> {
    if soc_family(soc) != Some(SocFamily::Rockchip) {
        return None;
    }
    let (bank, line) = rockchip_bank_line(gpio)?;
    let pin = rockchip_pin_name(bank, line);
    let soc = soc.trim();
    Some(format!(
        "/dts-v1/;\n\
         /plugin/;\n\
         \n\
         /*\n \
         * Written by the ADOS installer: PPS input from the GNSS receiver on\n \
         * {soc} {pin} (Linux GPIO {gpio}). Not yet validated on hardware.\n \
         */\n\
         \n\
         / {{\n\
         \tfragment@0 {{\n\
         \t\ttarget-path = \"/\";\n\
         \t\t__overlay__ {{\n\
         \t\t\tpps {{\n\
         \t\t\t\tcompatible = \"pps-gpio\";\n\
         \t\t\t\tgpios = <&gpio{bank} {line} 0>; /* {pin}, GPIO_ACTIVE_HIGH */\n\
         \t\t\t\tstatus = \"okay\";\n\
         \t\t\t}};\n\
         \t\t}};\n\
         \t}};\n\
         }};\n"
    ))
}

/// The Pi boot-config line for the firmware's stock `pps-gpio` overlay.
pub fn pi_pps_line(gpio: i64) -> String {
    format!("dtoverlay=pps-gpio,gpiopin={gpio}")
}

/// Rewrite a Pi `config.txt` so it loads the stock `pps-gpio` overlay (pure).
///
/// Unchanged when any active `dtoverlay=pps-gpio` line exists: an operator's
/// own PPS line wins. Otherwise the line is appended, under an `[all]` header
/// when the file ends inside a model-conditional section.
pub fn ensure_pi_pps(cfg: &str, gpio: i64) -> String {
    let active = cfg
        .lines()
        .map(str::trim)
        .any(|l| l == "dtoverlay=pps-gpio" || l.starts_with("dtoverlay=pps-gpio,"));
    if active {
        return cfg.to_string();
    }
    let last_section = cfg
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| l.starts_with('[') && l.ends_with(']'));
    let mut out = cfg.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if last_section.is_some_and(|s| !s.eq_ignore_ascii_case("[all]")) {
        out.push_str("[all]\n");
    }
    out.push_str(&pi_pps_line(gpio));
    out.push('\n');
    out
}

/// Add `name` to the `user_overlays=` line of an Armbian `armbianEnv.txt`
/// (pure). Unchanged when the name is already listed; a line is appended
/// when none exists.
pub fn ensure_user_overlay(env: &str, name: &str) -> String {
    let mut found = false;
    let mut lines: Vec<String> = Vec::new();
    for raw in env.lines() {
        match raw.trim().strip_prefix("user_overlays=") {
            Some(value) if !found => {
                found = true;
                if value.split_whitespace().any(|t| t == name) {
                    return env.to_string();
                }
                let value = value.trim();
                lines.push(if value.is_empty() {
                    format!("user_overlays={name}")
                } else {
                    format!("user_overlays={value} {name}")
                });
            }
            _ => lines.push(raw.to_string()),
        }
    }
    if !found {
        lines.push(format!("user_overlays={name}"));
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// `managed.list` with `entry` registered, or `None` when it already is (pure).
pub fn ensure_managed_entry(list: &str, entry: &str) -> Option<String> {
    if list.lines().any(|l| l.trim() == entry) {
        return None;
    }
    let mut out = list.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(entry);
    out.push('\n');
    Some(out)
}

/// The image's overlay mechanism, detected from the boot partition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootMechanism {
    /// Radxa vendor image: `dtbo/managed.list` + `u-boot-update`.
    RadxaManaged,
    /// Armbian: `armbianEnv.txt` `user_overlays=`.
    Armbian,
}

/// Detect the Rockchip overlay mechanism under `boot`, in the display
/// installer's priority order. `None` = unknown, so nothing is touched.
pub fn detect_mechanism(boot: &Path, has_uboot_update: bool) -> Option<BootMechanism> {
    if boot.join("dtbo/managed.list").is_file() && has_uboot_update {
        Some(BootMechanism::RadxaManaged)
    } else if boot.join("armbianEnv.txt").is_file() {
        Some(BootMechanism::Armbian)
    } else {
        None
    }
}

/// What a provisioning pass did.
#[derive(Debug, PartialEq, Eq)]
pub enum Provisioned {
    /// The overlay is newly enabled; a reboot binds it.
    Staged,
    /// Already enabled; nothing changed.
    AlreadyEnabled,
    /// Not applied, for the logged reason; the boot config is untouched.
    NotApplied(String),
}

/// Copy `dtbo` to `dest` unless identical bytes are already there. Returns
/// whether the file changed.
fn install_dtbo(dtbo: &[u8], dest: &Path) -> Result<bool, String> {
    if std::fs::read(dest).is_ok_and(|cur| cur == dtbo) {
        return Ok(false);
    }
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    std::fs::write(dest, dtbo).map_err(|e| format!("write {}: {e}", dest.display()))?;
    Ok(true)
}

/// Radxa vendor image: install into `dtbo/`, register in `managed.list`, and
/// regenerate `extlinux.conf` with `uboot_update`, restoring it if the
/// regeneration fails or truncates it.
pub fn enable_radxa(
    boot: &Path,
    dtbo: &[u8],
    uboot_update: impl Fn() -> bool,
) -> Result<Provisioned, String> {
    let dtbo_dir = boot.join("dtbo");
    let entry = format!("{OVERLAY_NAME}.dtbo");
    let mut changed = install_dtbo(dtbo, &dtbo_dir.join(&entry))?;
    let disabled = dtbo_dir.join(format!("{entry}.disabled"));
    if disabled.exists() {
        std::fs::remove_file(&disabled)
            .map_err(|e| format!("remove {}: {e}", disabled.display()))?;
        changed = true;
    }
    let list_path = dtbo_dir.join("managed.list");
    let list = std::fs::read_to_string(&list_path).unwrap_or_default();
    if let Some(updated) = ensure_managed_entry(&list, &entry) {
        std::fs::write(&list_path, updated)
            .map_err(|e| format!("write {}: {e}", list_path.display()))?;
        changed = true;
    }

    let extlinux = boot.join("extlinux/extlinux.conf");
    let referenced = std::fs::read_to_string(&extlinux).is_ok_and(|c| c.contains(OVERLAY_NAME));
    if !changed && referenced {
        return Ok(Provisioned::AlreadyEnabled);
    }

    teardown::regenerate_extlinux(&extlinux, uboot_update)?;
    Ok(Provisioned::Staged)
}

/// Armbian: install into `overlay-user/` and name it in `user_overlays=`.
pub fn enable_armbian(boot: &Path, dtbo: &[u8]) -> Result<Provisioned, String> {
    let installed = install_dtbo(
        dtbo,
        &boot.join(format!("overlay-user/{OVERLAY_NAME}.dtbo")),
    )?;
    let env = boot.join("armbianEnv.txt");
    match boot_config::edit_boot_config_at(&[env.as_path()], "pps-gpio overlay", |c| {
        ensure_user_overlay(c, OVERLAY_NAME)
    }) {
        BootConfigEdit::Written(_) => Ok(Provisioned::Staged),
        BootConfigEdit::Unchanged if installed => Ok(Provisioned::Staged),
        BootConfigEdit::Unchanged => Ok(Provisioned::AlreadyEnabled),
        BootConfigEdit::Absent => Ok(Provisioned::NotApplied(format!(
            "{} not found",
            env.display()
        ))),
        BootConfigEdit::Refused(msg) => Err(msg),
    }
}

/// Compile `dts` with `dtc -@`. `Ok(None)` when `dtc` is not installed.
fn compile_dts(dts: &str) -> Result<Option<Vec<u8>>, String> {
    if !on_path("dtc") {
        return Ok(None);
    }
    let dir = tempfile::tempdir().map_err(|e| format!("temp dir: {e}"))?;
    let src = dir.path().join(format!("{OVERLAY_NAME}.dts"));
    let out = dir.path().join(format!("{OVERLAY_NAME}.dtbo"));
    std::fs::write(&src, dts).map_err(|e| format!("write {}: {e}", src.display()))?;
    let (src_s, out_s) = (src.to_string_lossy(), out.to_string_lossy());
    let res = exec::run(
        "dtc",
        &["-@", "-I", "dts", "-O", "dtb", "-o", &out_s, &src_s],
    );
    if !res.success() {
        return Err(format!("dtc failed: {}", res.stderr.trim()));
    }
    std::fs::read(&out)
        .map(Some)
        .map_err(|e| format!("read {}: {e}", out.display()))
}

/// Provision the PPS overlay for `gpio` on `soc` against the live boot
/// partition.
fn provision(soc: &str, gpio: i64) -> Result<Provisioned, String> {
    match soc_family(soc) {
        Some(SocFamily::Broadcom) => {
            if !(0..=PI_MAX_GPIO).contains(&gpio) {
                return Ok(Provisioned::NotApplied(format!(
                    "GPIO {gpio} is not a Pi header line"
                )));
            }
            match boot_config::edit_boot_config("pps-gpio overlay", |c| ensure_pi_pps(c, gpio)) {
                BootConfigEdit::Written(_) => Ok(Provisioned::Staged),
                BootConfigEdit::Unchanged => Ok(Provisioned::AlreadyEnabled),
                BootConfigEdit::Absent => {
                    Ok(Provisioned::NotApplied("no Pi boot config found".into()))
                }
                BootConfigEdit::Refused(msg) => Err(msg),
            }
        }
        Some(SocFamily::Rockchip) => {
            let boot = Path::new(BOOT_DIR);
            let Some(mechanism) = detect_mechanism(boot, on_path("u-boot-update")) else {
                return Ok(Provisioned::NotApplied(
                    "unrecognised overlay mechanism on this image".into(),
                ));
            };
            let Some(dts) = pps_overlay_dts(soc, gpio) else {
                return Ok(Provisioned::NotApplied(format!(
                    "GPIO {gpio} is outside the {soc} GPIO banks"
                )));
            };
            let Some(dtbo) = compile_dts(&dts)? else {
                return Ok(Provisioned::NotApplied(
                    "dtc not installed (package device-tree-compiler)".into(),
                ));
            };
            match mechanism {
                BootMechanism::RadxaManaged => {
                    enable_radxa(boot, &dtbo, || exec::run_ok("u-boot-update", &[]))
                }
                BootMechanism::Armbian => enable_armbian(boot, &dtbo),
            }
        }
        None => Ok(Provisioned::NotApplied(format!(
            "no PPS overlay method for SoC {soc}"
        ))),
    }
}

/// Merge `pps-gpio` into the shared modules-load file and load it now.
fn ensure_pps_module() {
    let path = Path::new(MODULES_LOAD_FILE);
    let body = std::fs::read_to_string(path).unwrap_or_default();
    if !body.lines().any(|l| l.trim() == PPS_MODULE) {
        let mut out = body;
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(PPS_MODULE);
        out.push('\n');
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(path, out) {
            tracing::warn!(error = %e, path = MODULES_LOAD_FILE, "could not register pps-gpio module");
        }
    }
    let _ = exec::run("modprobe", &[PPS_MODULE]);
}

/// Enable the board's declared GNSS PPS line as a kernel PPS source.
pub struct PpsOverlay;

impl Step for PpsOverlay {
    fn id(&self) -> &str {
        "pps_overlay"
    }
    fn requires(&self) -> &[&str] {
        // After config_identity, so the display installer's pristine boot-config
        // snapshot is taken before this step edits the same file.
        &["deps", "config_identity"]
    }
    fn checkpoint(&self) -> Option<&str> {
        // No checkpoint: re-affirm on every upgrade (idempotent).
        None
    }
    fn kind(&self) -> StepKind {
        // A PPS problem degrades the install, never aborts it.
        StepKind::Optional
    }
    fn run(&self, ctx: &mut Ctx) -> StepOutcome {
        let profiles = load_profiles();
        let facts = probe_host_facts();
        let Some(matched) = resolve_profile(&profiles, &facts) else {
            return StepOutcome::Skipped;
        };
        let profile = matched.profile;
        let Some(gpio) = gate::pps_gpio_for(&ctx.profile, profile.gnss.as_ref()) else {
            tracing::info!(profile = %ctx.profile, board = %profile.stem, "pps-gpio overlay not needed on this node");
            return StepOutcome::Skipped;
        };
        match provision(&profile.soc, gpio) {
            Ok(Provisioned::Staged) => {
                tracing::info!(board = %profile.stem, gpio, "pps-gpio overlay staged (reboot to apply)");
                crate::steps::reboot::signal_required(&format!("pps-overlay {OVERLAY_NAME}"));
                ensure_pps_module();
                StepOutcome::Ok
            }
            Ok(Provisioned::AlreadyEnabled) => {
                ensure_pps_module();
                StepOutcome::Ok
            }
            Ok(Provisioned::NotApplied(reason)) => {
                tracing::warn!(board = %profile.stem, gpio, %reason, "pps-gpio overlay not applied; boot config unchanged");
                StepOutcome::Skipped
            }
            Err(msg) => StepOutcome::Failed(format!("pps-gpio overlay: {msg}")),
        }
    }
}

mod gate;
mod teardown;

pub use teardown::remove_overlay;

#[cfg(test)]
mod tests;
