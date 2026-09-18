//! The ONE place Rust edits a Raspberry-Pi boot config.
//!
//! Brick-safety on every boot-critical overlay rests on a single invariant:
//! `<cfg>.ados-bak` holds the **pristine, pre-install** boot config. The
//! display installer (`scripts/drivers/install-display-overlay.sh`) writes that
//! snapshot before it applies a panel overlay, arms `/etc/ados/display.probation`,
//! and the boot-time probe restores that exact file when the panel never binds.
//!
//! Each Rust boot-config editor used to take its own `std::fs::write(&bak, …)`
//! snapshot, unconditionally. A later step (`i2c_enable`, `purge_residue`) then
//! overwrote the pristine baseline with a config that already carried the panel
//! overlay — so the auto-revert "restored" a config with `dtoverlay=waveshare35a`
//! still in it and `dtoverlay=vc4-kms-v3d` still commented out: no KMS, no
//! `/dev/dri`, no kiosk, no panel. A dark ground station recoverable only by SSH
//! or a card reader.
//!
//! So every Rust edit goes through [`edit_boot_config`], and the snapshot is
//! **keep-first** — the same semantics the shell helper documents ("keeps the
//! FIRST (pristine) snapshot if one already exists"). An edit that cannot be
//! snapshotted is refused, never applied blind.

use std::path::{Path, PathBuf};

/// The Pi boot-config candidates, current image first.
pub const PI_CONFIG_PATHS: &[&str] = &["/boot/firmware/config.txt", "/boot/config.txt"];

/// What an [`edit_boot_config`] call did.
#[derive(Debug, PartialEq, Eq)]
pub enum BootConfigEdit {
    /// No boot config exists at any candidate path (not a Pi-family board).
    Absent,
    /// The transform produced no change — already in the wanted state.
    Unchanged,
    /// The file was rewritten; takes effect on the next reboot.
    Written(PathBuf),
    /// The snapshot or the write failed; the config is untouched.
    Refused(String),
}

/// Snapshot `path` to `<path>.ados-bak`, **keeping an existing snapshot**.
///
/// Keep-first is the whole point: a second snapshot would replace the pristine
/// pre-install baseline with an already-edited config and silently disarm the
/// display probe's auto-revert.
pub fn snapshot_boot_config(path: &Path) -> std::io::Result<PathBuf> {
    let mut bak = path.as_os_str().to_os_string();
    bak.push(".ados-bak");
    let bak = PathBuf::from(bak);
    if bak.exists() {
        return Ok(bak);
    }
    std::fs::copy(path, &bak)?;
    tracing::info!(snapshot = %bak.display(), "saved pristine boot-config snapshot");
    Ok(bak)
}

/// Apply a pure transform to the first boot config that exists among `paths`,
/// snapshotting keep-first before the write.
///
/// `reason` names the edit in the log. Returns what happened so the caller can
/// decide whether a reboot is now required.
pub fn edit_boot_config_at(
    paths: &[&Path],
    reason: &str,
    transform: impl Fn(&str) -> String,
) -> BootConfigEdit {
    for path in paths {
        let current = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let updated = transform(&current);
        if updated == current {
            return BootConfigEdit::Unchanged;
        }
        if let Err(e) = snapshot_boot_config(path) {
            // Fail closed: without a restorable baseline the display probe
            // cannot auto-revert, so refuse the edit rather than leave a board
            // that can only be recovered with a card reader.
            let msg = format!("could not snapshot {}: {e}", path.display());
            tracing::warn!(reason, error = %e, cfg = %path.display(), "refusing boot-config edit without a snapshot");
            return BootConfigEdit::Refused(msg);
        }
        if let Err(e) = std::fs::write(path, &updated) {
            tracing::warn!(reason, error = %e, cfg = %path.display(), "boot-config write failed");
            return BootConfigEdit::Refused(format!("write {} failed: {e}", path.display()));
        }
        tracing::info!(reason, cfg = %path.display(), "boot config edited (reboot to apply)");
        return BootConfigEdit::Written(path.to_path_buf());
    }
    BootConfigEdit::Absent
}

/// [`edit_boot_config_at`] over the real Pi boot-config candidates.
pub fn edit_boot_config(reason: &str, transform: impl Fn(&str) -> String) -> BootConfigEdit {
    let paths: Vec<&Path> = PI_CONFIG_PATHS.iter().map(Path::new).collect();
    edit_boot_config_at(&paths, reason, transform)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, body: &str) {
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn snapshot_keeps_the_first_pristine_copy() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.txt");
        write(&cfg, "pristine\n");

        let bak = snapshot_boot_config(&cfg).unwrap();
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), "pristine\n");

        // A later step edits the live config and snapshots again: the baseline
        // must still be the pre-install bytes, not the edited ones.
        write(&cfg, "pristine\ndtoverlay=waveshare35a\n");
        let bak2 = snapshot_boot_config(&cfg).unwrap();
        assert_eq!(bak2, bak);
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), "pristine\n");
    }

    #[test]
    fn two_sequential_edits_keep_the_pristine_baseline() {
        // The real failure: the display installer snapshots, then a later Rust
        // step edits the same file. The probe must still be able to restore a
        // config with no panel overlay in it.
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.txt");
        write(&cfg, "dtoverlay=vc4-kms-v3d\n");
        let paths = [cfg.as_path()];

        let first = edit_boot_config_at(&paths, "panel overlay", |c| {
            format!("{c}dtoverlay=waveshare35a\n")
        });
        assert!(matches!(first, BootConfigEdit::Written(_)));

        let second = edit_boot_config_at(&paths, "i2c", |c| format!("{c}dtparam=i2c_arm=on\n"));
        assert!(matches!(second, BootConfigEdit::Written(_)));

        let bak = std::fs::read_to_string(cfg.with_extension("txt.ados-bak")).unwrap();
        assert_eq!(bak, "dtoverlay=vc4-kms-v3d\n");
        assert!(!bak.contains("waveshare35a"));
        assert!(!bak.contains("i2c_arm"));
    }

    #[test]
    fn unchanged_transform_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.txt");
        write(&cfg, "dtparam=i2c_arm=on\n");
        let paths = [cfg.as_path()];
        assert_eq!(
            edit_boot_config_at(&paths, "i2c", |c| c.to_string()),
            BootConfigEdit::Unchanged
        );
        // No snapshot is taken for a no-op edit.
        assert!(!cfg.with_extension("txt.ados-bak").exists());
    }

    #[test]
    fn absent_config_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.txt");
        let paths = [missing.as_path()];
        assert_eq!(
            edit_boot_config_at(&paths, "i2c", |c| format!("{c}x")),
            BootConfigEdit::Absent
        );
    }

    #[test]
    fn first_existing_candidate_wins() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("firmware-config.txt");
        let present = dir.path().join("config.txt");
        write(&present, "a\n");
        let paths = [missing.as_path(), present.as_path()];
        let out = edit_boot_config_at(&paths, "i2c", |c| format!("{c}b\n"));
        assert_eq!(out, BootConfigEdit::Written(present.clone()));
        assert_eq!(std::fs::read_to_string(&present).unwrap(), "a\nb\n");
        assert!(!missing.exists());
    }
}
