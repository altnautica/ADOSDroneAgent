//! Boot-time display presence probe with apply-verify-auto-revert.
//!
//! Ports `display_probe.py`. Fires once per boot (a systemd oneshot) only while
//! a display overlay is on probation: an SPI-LCD overlay was written to the boot
//! config before the panel could be confirmed present.
//!
//! * CONFIRM: the panel bound this boot (a framebuffer reports the expected
//!   fbtft driver and, when the panel has touch, the touch controller shows up
//!   as an input device). Clear probation, write the persistent enabled marker.
//! * AUTO-REVERT: the panel never bound. Restore the boot config from the
//!   install-time snapshot (only when it is at least 100 bytes — the truncation
//!   guard), rewrite display.conf to a disabled block, and clear the markers.
//!
//! The probe writes the boot config ONLY to restore a known-good snapshot, and
//! only on the revert path. Pure sysfs + file IO with injectable roots so the
//! whole confirm/revert decision is testable against temp trees.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::fb_geometry::is_spi_lcd_driver;

/// How long to wait for the SPI LCD framebuffer console to bind. fbtft binds
/// late in kernel boot; poll past the slowest observed board before judging the
/// panel absent.
pub const BIND_POLL_SECONDS: f64 = 20.0;
/// Cadence of the bind poll.
pub const BIND_POLL_INTERVAL_SECONDS: f64 = 0.5;

/// A valid boot config snapshot is larger than this; a smaller one is treated as
/// truncated and never restored over a working boot config.
pub const MIN_SNAPSHOT_BYTES: usize = 100;

/// Filesystem roots the probe reads + writes. Defaulted to the real paths;
/// overridden to temp trees in tests.
#[derive(Debug, Clone)]
pub struct ProbePaths {
    pub sys_graphics_dir: PathBuf,
    pub sys_input_dir: PathBuf,
    pub display_conf: PathBuf,
    pub display_enabled: PathBuf,
    pub display_probation: PathBuf,
}

impl Default for ProbePaths {
    fn default() -> Self {
        Self {
            sys_graphics_dir: PathBuf::from(crate::fb_geometry::SYS_GRAPHICS_DIR),
            sys_input_dir: PathBuf::from("/sys/class/input"),
            display_conf: PathBuf::from(crate::conf::DISPLAY_CONF_PATH),
            display_enabled: PathBuf::from("/etc/ados/display.enabled"),
            display_probation: PathBuf::from("/etc/ados/display.probation"),
        }
    }
}

/// The terminal outcome of a probe run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// No probation marker present — nothing to do.
    NoProbation,
    /// Panel bound; probation cleared, overlay retained.
    Confirmed,
    /// Panel never bound; boot config restored (if a valid snapshot existed)
    /// and the display disabled.
    Reverted { boot_config_restored: bool },
}

/// Parse a `key=value` marker file (probation / enabled). Empty on missing.
pub fn parse_marker(path: &Path) -> BTreeMap<String, String> {
    crate::conf::parse(path)
}

/// Return the fb device name whose driver matches the expected panel, walking
/// every `<sys_graphics>/fb*` and matching by NAME (the SPI LCD lands on fb0
/// headless, fb1 when a DRM/HDMI driver claims a node). Prefers `expected_name`;
/// falls back to any known SPI-LCD driver name.
pub fn fb_bound(sys_graphics: &Path, expected_name: &str) -> Option<String> {
    if !sys_graphics.is_dir() {
        return None;
    }
    let mut entries: Vec<String> = match std::fs::read_dir(sys_graphics) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| is_fb_entry(n))
            .collect(),
        Err(_) => return None,
    };
    entries.sort();
    let mut fallback: Option<String> = None;
    for entry in entries {
        let Ok(fb_name) = std::fs::read_to_string(sys_graphics.join(&entry).join("name")) else {
            continue;
        };
        let fb_name = fb_name.trim();
        if !expected_name.is_empty() && fb_name.contains(expected_name) {
            return Some(entry);
        }
        if is_spi_lcd_driver(fb_name) {
            fallback = Some(entry);
        }
    }
    fallback
}

/// Is the panel's resistive touch controller present as an input device?
/// Returns true when no touch chip is expected (the framebuffer alone confirms
/// the panel) or when a matching `<sys_input>/event*/device/name` is found.
pub fn touch_bound(sys_input: &Path, touch_chip: &str) -> bool {
    let token = touch_chip.trim().to_lowercase();
    if token.is_empty() {
        return true;
    }
    if !sys_input.is_dir() {
        return false;
    }
    let mut entries: Vec<String> = match std::fs::read_dir(sys_input) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.starts_with("event"))
            .collect(),
        Err(_) => return false,
    };
    entries.sort();
    for entry in entries {
        let name_file = sys_input.join(&entry).join("device").join("name");
        let Ok(dev_name) = std::fs::read_to_string(&name_file) else {
            continue;
        };
        if dev_name.trim().to_lowercase().contains(&token) {
            return true;
        }
    }
    false
}

/// Confirm the panel: a matched framebuffer AND (if expected) the touch chip.
/// Returns the matched fb device name on success.
pub fn panel_present(
    sys_graphics: &Path,
    sys_input: &Path,
    expected_name: &str,
    touch_chip: &str,
) -> Option<String> {
    let fb = fb_bound(sys_graphics, expected_name)?;
    if !touch_bound(sys_input, touch_chip) {
        return None;
    }
    Some(fb)
}

/// Panel confirmed: write the persistent enabled marker and clear probation.
pub fn confirm(paths: &ProbePaths) -> std::io::Result<()> {
    if let Some(parent) = paths.display_enabled.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&paths.display_enabled, b"")?;
    let _ = std::fs::remove_file(&paths.display_probation);
    Ok(())
}

/// Panel never bound: restore the boot-config snapshot (only when >= the
/// truncation floor), rewrite display.conf to the disabled block, and clear the
/// markers. Returns whether the boot config was actually restored.
pub fn revert(paths: &ProbePaths, marker: &BTreeMap<String, String>) -> std::io::Result<bool> {
    let snapshot = marker.get("snapshot").map(|s| s.trim()).unwrap_or("");
    let boot_config = marker.get("boot_config").map(|s| s.trim()).unwrap_or("");
    let mut restored = false;
    if !snapshot.is_empty() && !boot_config.is_empty() {
        let snap_path = Path::new(snapshot);
        let boot_path = Path::new(boot_config);
        if snap_path.is_file() {
            // Never restore an empty/truncated snapshot over a working config.
            let data = std::fs::read(snap_path)?;
            if data.len() >= MIN_SNAPSHOT_BYTES {
                std::fs::write(boot_path, &data)?;
                restored = true;
            } else {
                tracing::warn!(
                    bytes = data.len(),
                    snapshot = snapshot,
                    "display probe snapshot too small to restore"
                );
            }
        }
    }

    // Rewrite display.conf so the UI service + heartbeat see "no display".
    if let Some(parent) = paths.display_conf.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let board = marker.get("board").map(|s| s.as_str()).unwrap_or("");
    let body = format!(
        "# Written by the display probe after an unconfirmed SPI-LCD overlay\n\
         # failed to bind. The boot config was restored from the install-time\n\
         # snapshot and the display disabled.\n\
         display_id=none\n\
         board={board}\n\
         has_touch=false\n\
         display_presence=reverted\n"
    );
    std::fs::write(&paths.display_conf, body)?;

    let _ = std::fs::remove_file(&paths.display_enabled);
    let _ = std::fs::remove_file(&paths.display_probation);
    Ok(restored)
}

/// Decide confirm vs revert for an already-bound check (no polling). Pure: takes
/// the panel-present result so the whole branch is testable without sleeping.
/// Returns the outcome after performing the side effects.
pub fn apply_decision(
    paths: &ProbePaths,
    marker: &BTreeMap<String, String>,
    fb: Option<String>,
) -> std::io::Result<ProbeOutcome> {
    if fb.is_some() {
        confirm(paths)?;
        Ok(ProbeOutcome::Confirmed)
    } else {
        let restored = revert(paths, marker)?;
        Ok(ProbeOutcome::Reverted {
            boot_config_restored: restored,
        })
    }
}

/// Whether `name` is an `fbN` entry with an all-digit suffix.
fn is_fb_entry(name: &str) -> bool {
    name.strip_prefix("fb")
        .map(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
        .unwrap_or(false)
}

/// Run the probe: no-op when no probation marker exists; otherwise poll for the
/// panel up to the late-bind window, then confirm or revert. Always returns Ok.
/// The poll uses the real clock; the decision branches are factored into
/// [`apply_decision`] for direct testing.
pub fn run(paths: &ProbePaths) -> std::io::Result<ProbeOutcome> {
    if !paths.display_probation.exists() {
        tracing::info!("display probe no-op: no probation marker");
        return Ok(ProbeOutcome::NoProbation);
    }
    let marker = parse_marker(&paths.display_probation);
    let expected_name = marker
        .get("expected_fb_name")
        .map(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("fb_ili9486")
        .to_string();
    let touch_chip = marker.get("touch_chip").map(|s| s.as_str()).unwrap_or("");

    let fb = wait_for_panel(paths, &expected_name, touch_chip);
    apply_decision(paths, &marker, fb)
}

/// Poll for the panel to bind, up to the late-bind window.
fn wait_for_panel(paths: &ProbePaths, expected_name: &str, touch_chip: &str) -> Option<String> {
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs_f64(BIND_POLL_SECONDS);
    loop {
        if let Some(fb) = panel_present(
            &paths.sys_graphics_dir,
            &paths.sys_input_dir,
            expected_name,
            touch_chip,
        ) {
            return Some(fb);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_secs_f64(
            BIND_POLL_INTERVAL_SECONDS,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_paths(dir: &Path) -> ProbePaths {
        ProbePaths {
            sys_graphics_dir: dir.join("sys/class/graphics"),
            sys_input_dir: dir.join("sys/class/input"),
            display_conf: dir.join("etc/ados/display.conf"),
            display_enabled: dir.join("etc/ados/display.enabled"),
            display_probation: dir.join("etc/ados/display.probation"),
        }
    }

    fn write_fb(sys_graphics: &Path, fb: &str, name: &str) {
        let d = sys_graphics.join(fb);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("name"), format!("{name}\n")).unwrap();
    }

    fn write_touch(sys_input: &Path, ev: &str, name: &str) {
        let d = sys_input.join(ev).join("device");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("name"), format!("{name}\n")).unwrap();
    }

    #[test]
    fn fb_bound_prefers_expected_then_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let g = dir.path().join("sys/class/graphics");
        write_fb(&g, "fb0", "rockchip-drm");
        write_fb(&g, "fb1", "fb_ili9486");
        assert_eq!(fb_bound(&g, "fb_ili9486").as_deref(), Some("fb1"));
        // No expected -> falls back to the SPI-LCD set match.
        assert_eq!(fb_bound(&g, "").as_deref(), Some("fb1"));
        // Expected not present, no SPI-LCD driver either -> None.
        let dir2 = tempfile::tempdir().unwrap();
        let g2 = dir2.path().join("sys/class/graphics");
        write_fb(&g2, "fb0", "rockchip-drm");
        assert_eq!(fb_bound(&g2, "fb_ili9486"), None);
    }

    #[test]
    fn fb_bound_none_when_dir_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(fb_bound(&dir.path().join("nope"), "fb_ili9486"), None);
    }

    #[test]
    fn touch_bound_no_chip_is_always_true() {
        let dir = tempfile::tempdir().unwrap();
        assert!(touch_bound(&dir.path().join("sys/class/input"), ""));
        assert!(touch_bound(&dir.path().join("sys/class/input"), "  "));
    }

    #[test]
    fn touch_bound_matches_input_device_name() {
        let dir = tempfile::tempdir().unwrap();
        let i = dir.path().join("sys/class/input");
        write_touch(&i, "event0", "some keyboard");
        write_touch(&i, "event1", "ADS7846 Touchscreen");
        assert!(touch_bound(&i, "ads7846"));
        assert!(!touch_bound(&i, "ft5406"));
    }

    #[test]
    fn touch_bound_false_when_expected_but_dir_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!touch_bound(&dir.path().join("nope"), "ads7846"));
    }

    #[test]
    fn panel_present_requires_fb_and_touch() {
        let dir = tempfile::tempdir().unwrap();
        let g = dir.path().join("sys/class/graphics");
        let i = dir.path().join("sys/class/input");
        write_fb(&g, "fb1", "fb_ili9486");
        // Touch expected but absent -> not present.
        std::fs::create_dir_all(&i).unwrap();
        assert!(panel_present(&g, &i, "fb_ili9486", "ads7846").is_none());
        // Touch shows up -> present.
        write_touch(&i, "event0", "ADS7846 Touchscreen");
        assert_eq!(
            panel_present(&g, &i, "fb_ili9486", "ads7846").as_deref(),
            Some("fb1")
        );
        // No touch expected -> fb alone is enough.
        assert_eq!(
            panel_present(&g, &i, "fb_ili9486", "").as_deref(),
            Some("fb1")
        );
    }

    #[test]
    fn confirm_writes_enabled_and_clears_probation() {
        let dir = tempfile::tempdir().unwrap();
        let p = temp_paths(dir.path());
        std::fs::create_dir_all(p.display_probation.parent().unwrap()).unwrap();
        std::fs::write(&p.display_probation, "display_id=lcd35\n").unwrap();
        confirm(&p).unwrap();
        assert!(p.display_enabled.exists());
        assert!(!p.display_probation.exists());
    }

    #[test]
    fn revert_restores_a_valid_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let p = temp_paths(dir.path());
        std::fs::create_dir_all(dir.path().join("boot")).unwrap();
        let snap = dir.path().join("boot/extlinux.conf.snap");
        let boot = dir.path().join("boot/extlinux.conf");
        // A valid (>= 100 byte) snapshot, and a clobbered current config.
        let good = "x".repeat(150);
        std::fs::write(&snap, &good).unwrap();
        std::fs::write(&boot, "broken overlay config").unwrap();
        std::fs::create_dir_all(p.display_enabled.parent().unwrap()).unwrap();
        std::fs::write(&p.display_enabled, "").unwrap();
        std::fs::write(&p.display_probation, "x").unwrap();

        let mut marker = BTreeMap::new();
        marker.insert("snapshot".into(), snap.to_string_lossy().into_owned());
        marker.insert("boot_config".into(), boot.to_string_lossy().into_owned());
        marker.insert("board".into(), "rock-5c".into());

        let restored = revert(&p, &marker).unwrap();
        assert!(restored);
        assert_eq!(std::fs::read_to_string(&boot).unwrap(), good);
        // display.conf rewritten to the disabled block.
        let conf = std::fs::read_to_string(&p.display_conf).unwrap();
        assert!(conf.contains("display_id=none"));
        assert!(conf.contains("board=rock-5c"));
        assert!(conf.contains("display_presence=reverted"));
        // Markers cleared.
        assert!(!p.display_enabled.exists());
        assert!(!p.display_probation.exists());
    }

    #[test]
    fn revert_refuses_a_truncated_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let p = temp_paths(dir.path());
        std::fs::create_dir_all(dir.path().join("boot")).unwrap();
        let snap = dir.path().join("boot/extlinux.conf.snap");
        let boot = dir.path().join("boot/extlinux.conf");
        // A too-small snapshot (< 100 bytes) must NOT clobber the working config.
        std::fs::write(&snap, "tiny").unwrap();
        let working = "a working boot config that should survive".to_string();
        std::fs::write(&boot, &working).unwrap();

        let mut marker = BTreeMap::new();
        marker.insert("snapshot".into(), snap.to_string_lossy().into_owned());
        marker.insert("boot_config".into(), boot.to_string_lossy().into_owned());

        let restored = revert(&p, &marker).unwrap();
        assert!(!restored);
        // The working config is intact.
        assert_eq!(std::fs::read_to_string(&boot).unwrap(), working);
        // display.conf still rewritten to disabled even with no restore.
        assert!(std::fs::read_to_string(&p.display_conf)
            .unwrap()
            .contains("display_id=none"));
    }

    #[test]
    fn revert_with_no_snapshot_still_disables() {
        let dir = tempfile::tempdir().unwrap();
        let p = temp_paths(dir.path());
        let marker = BTreeMap::new(); // no snapshot/boot_config keys
        let restored = revert(&p, &marker).unwrap();
        assert!(!restored);
        assert!(std::fs::read_to_string(&p.display_conf)
            .unwrap()
            .contains("display_presence=reverted"));
    }

    #[test]
    fn apply_decision_confirms_or_reverts() {
        let dir = tempfile::tempdir().unwrap();
        let p = temp_paths(dir.path());
        let marker = BTreeMap::new();
        // fb present -> confirmed.
        let out = apply_decision(&p, &marker, Some("fb1".into())).unwrap();
        assert_eq!(out, ProbeOutcome::Confirmed);
        assert!(p.display_enabled.exists());
        // fb absent -> reverted (no snapshot).
        let out = apply_decision(&p, &marker, None).unwrap();
        assert_eq!(
            out,
            ProbeOutcome::Reverted {
                boot_config_restored: false
            }
        );
    }

    #[test]
    fn run_is_noop_without_probation_marker() {
        let dir = tempfile::tempdir().unwrap();
        let p = temp_paths(dir.path());
        assert_eq!(run(&p).unwrap(), ProbeOutcome::NoProbation);
    }

    #[test]
    fn run_confirms_immediately_when_panel_already_bound() {
        let dir = tempfile::tempdir().unwrap();
        let p = temp_paths(dir.path());
        // Panel already bound: probation present + fb reports the driver.
        std::fs::create_dir_all(p.display_probation.parent().unwrap()).unwrap();
        std::fs::write(&p.display_probation, "expected_fb_name=fb_ili9486\n").unwrap();
        write_fb(&p.sys_graphics_dir, "fb1", "fb_ili9486");
        // No touch chip in the marker, so fb alone confirms with no polling.
        assert_eq!(run(&p).unwrap(), ProbeOutcome::Confirmed);
        assert!(p.display_enabled.exists());
        assert!(!p.display_probation.exists());
    }
}
