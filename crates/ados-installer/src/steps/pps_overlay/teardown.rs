//! The uninstall's reverse of the PPS provisioning, and the `extlinux.conf`
//! regeneration both directions share.
//!
//! Every edit the step makes is undone the way it was made: the generated
//! dtbo leaves `dtbo/` (with its `managed.list` entry, then `u-boot-update`)
//! or `overlay-user/` (with its `user_overlays=` token), the Pi loses the
//! `dtoverlay=pps-gpio,gpiopin=N` line, and the shared modules-load file loses
//! `pps-gpio`. Idempotent: a box carrying none of it is left untouched.

use std::path::{Path, PathBuf};

use super::{BOOT_DIR, EXTLINUX_MIN_BYTES, MODULES_LOAD_FILE, OVERLAY_NAME, PPS_MODULE};
use crate::steps::boot_config::{self, BootConfigEdit};
use crate::steps::wfb_ng::on_path;
use ados_term::exec;

/// Regenerate `extlinux.conf` with `uboot_update`, restoring it when the
/// regeneration fails or truncates it. Takes the keep-first pristine snapshot
/// for the display probe plus this step's own pre-edit copy to restore from,
/// so a failed regeneration undoes only this edit and never rolls back a
/// panel overlay staged earlier.
pub(super) fn regenerate_extlinux(
    extlinux: &Path,
    uboot_update: impl Fn() -> bool,
) -> Result<(), String> {
    let restore = if extlinux.is_file() {
        boot_config::snapshot_boot_config(extlinux).map_err(|e| {
            format!(
                "refusing u-boot-update without a snapshot of {}: {e}",
                extlinux.display()
            )
        })?;
        let bak = PathBuf::from(format!("{}.{OVERLAY_NAME}.bak", extlinux.display()));
        std::fs::copy(extlinux, &bak).map_err(|e| format!("snapshot {}: {e}", bak.display()))?;
        Some(bak)
    } else {
        None
    };
    let restore_now = |why: &str| -> String {
        if let Some(bak) = &restore {
            let _ = std::fs::copy(bak, extlinux);
        }
        format!("{why}; extlinux.conf restored")
    };
    if !uboot_update() {
        return Err(restore_now("u-boot-update failed"));
    }
    if restore.is_some() {
        let size = std::fs::metadata(extlinux).map(|m| m.len()).unwrap_or(0);
        if size < EXTLINUX_MIN_BYTES {
            return Err(restore_now(&format!(
                "extlinux.conf truncated to {size} bytes"
            )));
        }
    }
    Ok(())
}

/// A Pi `config.txt` without the line [`super::pi_pps_line`] writes (pure).
/// Only that exact form (`dtoverlay=pps-gpio,gpiopin=<digits>`) is removed.
pub fn strip_pi_pps(cfg: &str) -> String {
    let ours = |l: &str| {
        l.trim()
            .strip_prefix("dtoverlay=pps-gpio,gpiopin=")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    };
    if !cfg.lines().any(ours) {
        return cfg.to_string();
    }
    let mut out: String = cfg
        .lines()
        .filter(|&l| !ours(l))
        .collect::<Vec<_>>()
        .join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

/// An Armbian `armbianEnv.txt` with `name` dropped from `user_overlays=`
/// (pure). The line goes when `name` was its only overlay; unchanged when
/// `name` is not listed.
pub fn strip_user_overlay(env: &str, name: &str) -> String {
    let listed = |l: &str| {
        l.trim()
            .strip_prefix("user_overlays=")
            .is_some_and(|v| v.split_whitespace().any(|t| t == name))
    };
    if !env.lines().any(listed) {
        return env.to_string();
    }
    let mut lines: Vec<String> = Vec::new();
    for raw in env.lines() {
        match raw.trim().strip_prefix("user_overlays=") {
            Some(value) if listed(raw) => {
                let rest: Vec<&str> = value.split_whitespace().filter(|t| *t != name).collect();
                if !rest.is_empty() {
                    lines.push(format!("user_overlays={}", rest.join(" ")));
                }
            }
            _ => lines.push(raw.to_string()),
        }
    }
    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

/// `managed.list` without `entry`, or `None` when it is not registered (pure).
pub fn strip_managed_entry(list: &str, entry: &str) -> Option<String> {
    if !list.lines().any(|l| l.trim() == entry) {
        return None;
    }
    let mut out: String = list
        .lines()
        .filter(|l| l.trim() != entry)
        .collect::<Vec<_>>()
        .join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    Some(out)
}

/// Remove `path`, reporting whether it was there.
fn remove_file(path: &Path) -> Result<bool, String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("remove {}: {e}", path.display())),
    }
}

/// Radxa vendor image: remove the dtbo (and a `.disabled` copy), deregister
/// it from `managed.list`, and regenerate `extlinux.conf` when anything
/// changed. Returns whether anything changed.
pub fn remove_radxa(boot: &Path, uboot_update: impl Fn() -> bool) -> Result<bool, String> {
    let dtbo_dir = boot.join("dtbo");
    let entry = format!("{OVERLAY_NAME}.dtbo");
    let mut changed = remove_file(&dtbo_dir.join(&entry))?;
    changed |= remove_file(&dtbo_dir.join(format!("{entry}.disabled")))?;
    let list_path = dtbo_dir.join("managed.list");
    let list = std::fs::read_to_string(&list_path).unwrap_or_default();
    if let Some(updated) = strip_managed_entry(&list, &entry) {
        std::fs::write(&list_path, updated)
            .map_err(|e| format!("write {}: {e}", list_path.display()))?;
        changed = true;
    }
    if changed {
        regenerate_extlinux(&boot.join("extlinux/extlinux.conf"), uboot_update)?;
    }
    Ok(changed)
}

/// Armbian: remove the dtbo from `overlay-user/` and its `user_overlays=`
/// token. Returns whether anything changed.
pub fn remove_armbian(boot: &Path) -> Result<bool, String> {
    let removed = remove_file(&boot.join(format!("overlay-user/{OVERLAY_NAME}.dtbo")))?;
    let env = boot.join("armbianEnv.txt");
    match boot_config::edit_boot_config_at(&[env.as_path()], "pps-gpio overlay removal", |c| {
        strip_user_overlay(c, OVERLAY_NAME)
    }) {
        BootConfigEdit::Written(_) => Ok(true),
        BootConfigEdit::Unchanged | BootConfigEdit::Absent => Ok(removed),
        BootConfigEdit::Refused(msg) => Err(msg),
    }
}

/// The shared modules-load body without `pps-gpio`, or `None` when it is not
/// listed (pure). The other installers' modules stay.
pub fn strip_pps_module(body: &str) -> Option<String> {
    strip_managed_entry(body, PPS_MODULE)
}

/// Undo the PPS provisioning on this box, whichever mechanism carried it.
/// Best effort: each failure is logged and the rest still runs.
pub fn remove_overlay() {
    let boot = Path::new(BOOT_DIR);
    let radxa = if boot.join("dtbo/managed.list").is_file() && on_path("u-boot-update") {
        remove_radxa(boot, || exec::run_ok("u-boot-update", &[]))
    } else {
        Ok(false)
    };
    let pi = match boot_config::edit_boot_config("pps-gpio overlay removal", strip_pi_pps) {
        BootConfigEdit::Written(_) => Ok(true),
        BootConfigEdit::Unchanged | BootConfigEdit::Absent => Ok(false),
        BootConfigEdit::Refused(msg) => Err(msg),
    };
    for (mechanism, result) in [
        ("radxa", radxa),
        ("armbian", remove_armbian(boot)),
        ("pi", pi),
    ] {
        match result {
            Ok(true) => tracing::info!(mechanism, "pps-gpio overlay removed (reboot to apply)"),
            Ok(false) => {}
            Err(e) => tracing::warn!(mechanism, error = %e, "pps-gpio overlay removal failed"),
        }
    }
    let modules = Path::new(MODULES_LOAD_FILE);
    let body = std::fs::read_to_string(modules).unwrap_or_default();
    if let Some(updated) = strip_pps_module(&body) {
        if let Err(e) = std::fs::write(modules, updated) {
            tracing::warn!(error = %e, path = MODULES_LOAD_FILE, "could not deregister pps-gpio module");
        }
    }
}

#[cfg(test)]
#[path = "teardown_tests.rs"]
mod tests;
