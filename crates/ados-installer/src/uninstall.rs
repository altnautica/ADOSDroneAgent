//! Full uninstall / purge path + GS→drone residue reversion.
//!
//! Mirrors the canonical removal list in `src/ados/cli/main.py:_uninstall_linux`
//! and the bash `do_uninstall`: stop + disable + remove every `ados-*` unit,
//! the `.wants` dropins + `multi-user.target.wants` links, the system dropins
//! (tmpfiles/sysctl/udev/modules-load/NetworkManager/logind/avahi), the
//! `/usr/local/bin/ados*` symlinks, then `daemon-reload` + `reset-failed` +
//! `udevadm reload`, and finally the `/opt/ados`, `/var/ados`, `/var/lib/ados`,
//! `/var/log/ados`, `/run/ados` trees + the MOTD; with `purge`, also
//! `/etc/ados`. Shares the residue reversion in [`crate::steps::purge_residue`]
//! (the orphan default route + the SPI-LCD boot config) so a GS→drone flip
//! leaves a clean box.

use std::path::{Path, PathBuf};

use crate::env;
use crate::exec;

/// The systemd directory all ados units + dropins live under.
const SYSTEMD_DIR: &str = "/etc/systemd/system";

/// The login-banner MOTD the install drops.
const MOTD_FILE: &str = "/etc/update-motd.d/30-ados";

/// The system dropin files the install lays down OUTSIDE `/opt/ados`. Pure +
/// listed here (not glob-discovered) exactly as the canonical CLI removal list
/// in `main.py:570-585`, so the two uninstall surfaces never drift.
pub fn dropin_files() -> Vec<&'static str> {
    vec![
        "/etc/tmpfiles.d/ados.conf",
        "/etc/tmpfiles.d/ados-plugins.conf",
        "/etc/tmpfiles.d/99-ados-usb-autosuspend.conf",
        "/etc/sysctl.d/99-ados-video.conf",
        "/etc/modules-load.d/ados-display.conf",
        "/etc/udev/rules.d/50-ados-uvc-no-autosuspend.rules",
        "/etc/udev/rules.d/99-ados-hardware.rules",
        "/etc/udev/rules.d/99-ados-input.rules",
        "/etc/udev/rules.d/99-ados-modem.rules",
        "/etc/udev/rules.d/99-ados-wifi-powersave.rules",
        "/etc/udev/rules.d/99-ados-usb-no-autosuspend.rules",
        "/etc/udev/rules.d/99-ados-eth-no-eee.rules",
        "/etc/NetworkManager/conf.d/99-ados-wifi-powersave.conf",
        "/etc/systemd/logind.conf.d/99-ados-nosleep.conf",
        "/etc/avahi/services/ados-gs-ap.service",
    ]
}

/// The `/usr/local/bin/ados*` symlinks the install creates.
fn symlinks() -> Vec<&'static str> {
    vec![
        "/usr/local/bin/ados",
        "/usr/local/bin/ados-agent",
        "/usr/local/bin/ados-supervisor",
    ]
}

/// Discover every `ados-*.{service,slice,target,timer}` unit file under the
/// systemd dir (glob, matching the CLI + bash uninstall).
fn discover_unit_files() -> Vec<PathBuf> {
    let dir = Path::new(SYSTEMD_DIR);
    let mut units: Vec<PathBuf> = Vec::new();
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return units,
    };
    for entry in read.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if !name.starts_with("ados-") {
            continue;
        }
        if name.ends_with(".service")
            || name.ends_with(".slice")
            || name.ends_with(".target")
            || name.ends_with(".timer")
        {
            // Only real unit files, not the `.wants` directories.
            if path.is_file() || path.is_symlink() {
                units.push(path);
            }
        }
    }
    units.sort();
    units
}

/// Discover the `ados-*.service.wants` dropin directories.
fn discover_wants_dirs() -> Vec<PathBuf> {
    let dir = Path::new(SYSTEMD_DIR);
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(read) = std::fs::read_dir(dir) {
        for entry in read.flatten() {
            let path = entry.path();
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.starts_with("ados-") && name.ends_with(".service.wants") && path.is_dir() {
                    dirs.push(path);
                }
            }
        }
    }
    dirs.sort();
    dirs
}

/// Discover the `multi-user.target.wants/ados-*` enable links.
fn discover_target_wants_links() -> Vec<PathBuf> {
    let dir = Path::new(SYSTEMD_DIR).join("multi-user.target.wants");
    let mut links: Vec<PathBuf> = Vec::new();
    if let Ok(read) = std::fs::read_dir(&dir) {
        for entry in read.flatten() {
            let path = entry.path();
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.starts_with("ados-") {
                    links.push(path);
                }
            }
        }
    }
    links.sort();
    links
}

/// Stop + disable + remove every discovered ados unit file.
fn remove_units(units: &[PathBuf]) {
    for unit in units {
        let name = match unit.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if name.ends_with(".service") {
            // Stop then disable; both harmless on a never-enabled unit.
            let _ = exec::run("systemctl", &["stop", name]);
            let _ = exec::run("systemctl", &["disable", name]);
        } else {
            // .slice / .target / .timer — best-effort stop.
            let _ = exec::run("systemctl", &["stop", name]);
        }
        if let Err(e) = remove_path(unit) {
            tracing::warn!(unit = name, error = %e, "removing unit file failed");
        }
    }
}

/// Remove a single file or symlink, ignoring a missing path.
fn remove_path(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Run the uninstall. `purge` additionally removes `/etc/ados` (device id,
/// pairing, config) for a from-clean reinstall.
pub fn run_uninstall(purge: bool) -> anyhow::Result<()> {
    // ── 1. Units: stop + disable + remove ──
    let units = discover_unit_files();
    remove_units(&units);

    // ── 2. Dropin .wants dirs + target enable links ──
    for wants in discover_wants_dirs() {
        let _ = std::fs::remove_dir_all(&wants);
    }
    for link in discover_target_wants_links() {
        if let Err(e) = remove_path(&link) {
            tracing::warn!(link = %link.display(), error = %e, "removing target link failed");
        }
    }

    // ── 3. System dropins outside /opt/ados ──
    for dropin in dropin_files() {
        let _ = remove_path(Path::new(dropin));
    }

    // ── 4. Reload systemd + udev so the removed units/rules are forgotten ──
    let _ = exec::run("systemctl", &["daemon-reload"]);
    let _ = exec::run("systemctl", &["reset-failed"]);
    let _ = exec::run("udevadm", &["control", "--reload-rules"]);

    // ── 5. Global symlinks ──
    for link in symlinks() {
        let _ = remove_path(Path::new(link));
    }

    // ── 6. State + data + install + log + runtime trees (always) ──
    // `/var/ados` (mutable data) and `/var/log/ados` are not in the env path
    // constants; the canonical CLI removal list names them as literals, so we
    // do the same here.
    for dir in [
        env::INSTALL_DIR,
        "/var/ados",
        env::STATE_DIR,
        "/var/log/ados",
        "/run/ados",
    ] {
        let _ = std::fs::remove_dir_all(dir);
    }
    let _ = remove_path(Path::new(MOTD_FILE));

    // ── 7. Config (only on --purge) ──
    if purge {
        let _ = std::fs::remove_dir_all(env::CONFIG_DIR);
    }

    // ── 8. Residue reversion so a GS→drone flip leaves a clean box ──
    crate::steps::purge_residue::revert_residue();

    tracing::info!(purge, "ADOS Drone Agent uninstalled");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropin_list_matches_the_canonical_removal_set() {
        let dropins = dropin_files();
        // The load-bearing ones the bash + CLI uninstall remove.
        for expected in [
            "/etc/tmpfiles.d/ados.conf",
            "/etc/tmpfiles.d/ados-plugins.conf",
            "/etc/sysctl.d/99-ados-video.conf",
            "/etc/modules-load.d/ados-display.conf",
            "/etc/NetworkManager/conf.d/99-ados-wifi-powersave.conf",
            "/etc/systemd/logind.conf.d/99-ados-nosleep.conf",
            "/etc/avahi/services/ados-gs-ap.service",
        ] {
            assert!(
                dropins.contains(&expected),
                "dropin set must include {expected}"
            );
        }
        // All udev rules are under rules.d.
        assert!(
            dropins
                .iter()
                .filter(|p| p.contains("/udev/rules.d/"))
                .count()
                >= 6
        );
    }

    #[test]
    fn symlink_set_includes_the_three_global_links() {
        let s = symlinks();
        assert!(s.contains(&"/usr/local/bin/ados"));
        assert!(s.contains(&"/usr/local/bin/ados-agent"));
        assert!(s.contains(&"/usr/local/bin/ados-supervisor"));
    }
}
