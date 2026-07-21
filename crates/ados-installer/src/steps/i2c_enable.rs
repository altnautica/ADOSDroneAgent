//! Enable the I2C bus on a Raspberry Pi so the optional I2C status OLED (and any
//! other I2C peripheral) is reachable at `/dev/i2c-1` after the next reboot.
//!
//! A fresh Raspberry Pi OS flash ships with I2C OFF (no `dtparam=i2c_arm=on` in
//! the boot config, no `i2c-dev` module). The ground-station I2C OLED service
//! gates on `/dev/i2c-1`, so without this step the OLED would silently never
//! appear on a fresh install (Rule 26: no manual `raspi-config` step).
//!
//! What it does (ground-station profile, Pi only):
//!   * ensures an active `dtparam=i2c_arm=on` line in the Pi boot config
//!     (uncomment a commented one, else append) — snapshotting to `<cfg>.ados-bak`
//!     first, exactly like the SPI-LCD residue path;
//!   * writes `/etc/modules-load.d/ados-i2c.conf` so `i2c-dev` loads every boot;
//!   * best-effort `modprobe i2c-dev` so the node appears without a reboot when
//!     the bus is already enabled in the device tree.
//!
//! The boot-config edit is purely ADDITIVE (it never removes or disables an
//! overlay), so it carries no brick risk — unlike the SPI-LCD overlay it only
//! turns a bus ON. Idempotent: a no-op when `dtparam=i2c_arm=on` is already
//! active (the founder's box, and every box after the first install). The
//! `dtparam` change needs a reboot to create the bus; the module-load + modprobe
//! cover the already-enabled case.

use std::path::Path;

use crate::ctx::Ctx;
use crate::exec;
use crate::graph::{Step, StepKind, StepOutcome};

/// The Pi boot-config candidates, current image first.
const PI_CONFIG_PATHS: &[&str] = &["/boot/firmware/config.txt", "/boot/config.txt"];
/// The modules-load drop-in that loads `i2c-dev` every boot.
const MODULES_LOAD_PATH: &str = "/etc/modules-load.d/ados-i2c.conf";
const MODULES_LOAD_BODY: &str =
    "# Written by ADOS: load the I2C userspace interface for the status OLED.\ni2c-dev\n";

/// Rewrite a Pi `config.txt` body so `dtparam=i2c_arm=on` is ACTIVE. Pure.
///
///   * an already-active `dtparam=i2c_arm=on` -> unchanged (idempotent),
///   * a commented `#dtparam=i2c_arm=on` (or `# dtparam=...`) -> uncommented,
///   * neither present -> the line is appended.
///
/// Only ever turns the bus ON; never removes or disables anything.
pub fn ensure_i2c_arm(cfg: &str) -> String {
    let had_trailing_newline = cfg.ends_with('\n');
    let mut out: Vec<String> = Vec::new();
    let mut active = false;

    for raw in cfg.lines() {
        let trimmed = raw.trim_start();
        // Already active — record it and leave it exactly as-is.
        if trimmed.starts_with("dtparam=i2c_arm=on") {
            active = true;
            out.push(raw.to_string());
            continue;
        }
        // A commented-out enable line -> uncomment it (preserve indentation).
        let stripped = trimmed.trim_start_matches('#').trim_start();
        if trimmed.starts_with('#') && stripped.starts_with("dtparam=i2c_arm=on") {
            let indent: String = raw.chars().take_while(|c| c.is_whitespace()).collect();
            out.push(format!("{indent}dtparam=i2c_arm=on"));
            active = true;
            continue;
        }
        out.push(raw.to_string());
    }

    if !active {
        out.push("dtparam=i2c_arm=on".to_string());
    }

    let mut joined = out.join("\n");
    if had_trailing_newline {
        joined.push('\n');
    }
    joined
}

/// Ensure the boot config enables the ARM I2C bus, snapshotting before any edit.
/// No-op when the config is absent (a non-Pi board) or already enabled.
fn provision_i2c_boot_config() {
    for cfg_path in PI_CONFIG_PATHS {
        let path = Path::new(cfg_path);
        let current = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let updated = ensure_i2c_arm(&current);
        if updated == current {
            tracing::info!(cfg = cfg_path, "I2C already enabled in the boot config");
            return;
        }
        let bak = format!("{cfg_path}.ados-bak");
        if let Err(e) = std::fs::write(&bak, &current) {
            tracing::warn!(error = %e, "could not snapshot boot config before I2C enable; skipping");
            return;
        }
        if let Err(e) = std::fs::write(path, &updated) {
            tracing::warn!(error = %e, "writing I2C-enabled boot config failed");
        } else {
            tracing::info!(
                cfg = cfg_path,
                "enabled I2C in the boot config (reboot to apply)"
            );
        }
        return;
    }
}

/// Enable the I2C bus so the ground-station status OLED can bind.
pub struct I2cEnable;

impl Step for I2cEnable {
    fn id(&self) -> &str {
        "i2c_enable"
    }
    fn requires(&self) -> &[&str] {
        &[]
    }
    fn checkpoint(&self) -> Option<&str> {
        // No checkpoint: re-affirm on every upgrade (idempotent).
        None
    }
    fn kind(&self) -> StepKind {
        // A write problem must degrade, never abort the install.
        StepKind::Optional
    }
    fn run(&self, ctx: &mut Ctx) -> StepOutcome {
        // Scoped to the ground station: the I2C status OLED is a GS peripheral.
        if ctx.profile != "ground_station" {
            return StepOutcome::Skipped;
        }
        provision_i2c_boot_config();
        // Load i2c-dev every boot + best-effort now (covers an already-enabled bus).
        if let Err(e) = std::fs::write(MODULES_LOAD_PATH, MODULES_LOAD_BODY) {
            tracing::warn!(error = %e, path = MODULES_LOAD_PATH, "could not write i2c modules-load drop-in");
        }
        let _ = exec::run("modprobe", &["i2c-dev"]);
        StepOutcome::Ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_when_absent() {
        let cfg = "dtparam=audio=on\ndtoverlay=vc4-kms-v3d\n";
        let out = ensure_i2c_arm(cfg);
        assert!(out.contains("\ndtparam=i2c_arm=on"));
        assert!(out.ends_with('\n'));
        // Nothing else was touched.
        assert!(out.contains("dtparam=audio=on"));
        assert!(out.contains("dtoverlay=vc4-kms-v3d"));
    }

    #[test]
    fn idempotent_when_already_active() {
        let cfg = "dtparam=i2c_arm=on\nmax_framebuffers=2\n";
        assert_eq!(ensure_i2c_arm(cfg), cfg);
    }

    #[test]
    fn uncomments_a_commented_enable() {
        assert_eq!(
            ensure_i2c_arm("#dtparam=i2c_arm=on\n"),
            "dtparam=i2c_arm=on\n"
        );
        assert_eq!(
            ensure_i2c_arm("  # dtparam=i2c_arm=on\n"),
            "  dtparam=i2c_arm=on\n"
        );
    }

    #[test]
    fn preserves_a_missing_trailing_newline() {
        // A config with no trailing newline stays that way when we append.
        let out = ensure_i2c_arm("dtparam=audio=on");
        assert_eq!(out, "dtparam=audio=on\ndtparam=i2c_arm=on");
    }
}
