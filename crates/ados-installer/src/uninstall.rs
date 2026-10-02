//! Full uninstall / purge path + GS→drone residue reversion.
//!
//! The one uninstall path: `ados uninstall` runs this through the installer
//! copy a successful install keeps at [`env::INSTALLED_INSTALLER`] (or through
//! a freshly fetched installer when that copy is absent). It stops + disables +
//! removes every `ados-*` unit, the `.wants` dropins + `multi-user.target.wants`
//! links, the system dropins (tmpfiles/sysctl/udev/modules-load/NetworkManager/
//! logind/avahi), the `/usr/local/bin/ados*` symlinks, then `daemon-reload` +
//! `reset-failed` + `udevadm reload`, and finally the `/opt/ados`, `/var/ados`,
//! `/var/lib/ados`, `/var/log/ados`, `/run/ados` trees + the MOTD; with
//! `purge`, also `/etc/ados`. Shares the residue reversion in
//! [`crate::steps::purge_residue`] (the orphan default route + the SPI-LCD boot
//! config) so a GS→drone flip leaves a clean box.

use std::path::{Path, PathBuf};

use crate::env;
use crate::exec;
use crate::graph::StepOutcome;
use crate::ui::ProgressSink;

/// The systemd directory all ados units + dropins live under.
const SYSTEMD_DIR: &str = "/etc/systemd/system";

/// The login-banner MOTD the install drops.
const MOTD_FILE: &str = "/etc/update-motd.d/30-ados";

/// Every unit the install masks, so removing the agent gives the box back.
///
/// Masking is not a file under `/opt/ados` and is not undone by deleting the
/// agent's drop-ins: a masked unit is a symlink to `/dev/null` in
/// `/etc/systemd/system`, and it outlives everything else here. Without this,
/// uninstalling left the machine a headless appliance that could not sleep and
/// would not start a desktop — with nothing left on it to explain why.
///
/// Kept in step with the two masking sites in `steps/appliance.rs`
/// (`sleep_targets`, `display_manager_units`); the symmetry test below asserts
/// this list covers both.
pub fn masked_units() -> Vec<&'static str> {
    vec![
        // The keep-awake masks, applied on every profile.
        "sleep.target",
        "suspend.target",
        "hibernate.target",
        "hybrid-sleep.target",
        "suspend-then-hibernate.target",
        // The ground-station display-manager stand-down. Unmasking one the
        // install never masked is a harmless no-op, so this does not need to
        // know which profile the box was.
        "display-manager.service",
        "lightdm.service",
    ]
}

/// The system dropin files the install lays down OUTSIDE `/opt/ados`. Pure and
/// listed explicitly (not glob-discovered) so a removal never reaches a file
/// the install did not write.
pub fn dropin_files() -> Vec<&'static str> {
    vec![
        "/etc/tmpfiles.d/ados.conf",
        "/etc/tmpfiles.d/ados-plugins.conf",
        "/etc/tmpfiles.d/ados-vision.conf",
        "/etc/tmpfiles.d/99-ados-usb-autosuspend.conf",
        "/etc/tmpfiles.d/99-ados-log-retention.conf",
        "/etc/sysctl.d/99-ados-video.conf",
        "/etc/sysctl.d/20-ados-resilience.conf",
        "/etc/systemd/journald.conf.d/10-ados-persistent.conf",
        "/etc/modules-load.d/ados-display.conf",
        "/etc/udev/rules.d/50-ados-uvc-no-autosuspend.rules",
        "/etc/udev/rules.d/99-ados-hardware.rules",
        "/etc/udev/rules.d/99-ados-input.rules",
        "/etc/udev/rules.d/99-ados-modem.rules",
        "/etc/udev/rules.d/99-ados-wifi-powersave.rules",
        "/etc/udev/rules.d/99-ados-usb-no-autosuspend.rules",
        "/etc/udev/rules.d/99-ados-eth-no-eee.rules",
        "/etc/NetworkManager/conf.d/99-ados-wifi-powersave.conf",
        "/etc/modprobe.d/ados-aic8800.conf",
        "/etc/systemd/logind.conf.d/90-ados-no-idle.conf",
        "/etc/avahi/services/ados-gs-ap.service",
    ]
}

/// The `/usr/local/bin/ados*` symlinks the install creates — read from the
/// install's own list so the two surfaces cannot drift.
fn symlinks() -> Vec<&'static str> {
    crate::steps::fetch_binaries::global_symlinks()
        .into_iter()
        .map(|(_, link)| link)
        .collect()
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

/// Discover the `ados-*.service.wants` and `ados-*.service.d` drop-in
/// directories (the latter holds the LAN-front `front.conf`).
fn discover_wants_dirs() -> Vec<PathBuf> {
    let dir = Path::new(SYSTEMD_DIR);
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(read) = std::fs::read_dir(dir) {
        for entry in read.flatten() {
            let path = entry.path();
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.starts_with("ados-")
                    && (name.ends_with(".service.wants") || name.ends_with(".service.d"))
                    && path.is_dir()
                {
                    dirs.push(path);
                }
            }
        }
    }
    dirs.sort();
    dirs
}

/// The MAC-pin `.link` drop-ins under `dir`. Without removing them the box
/// keeps the pinned MACs on its next boot after the agent is gone.
fn discover_mac_pin_links(dir: &Path) -> Vec<PathBuf> {
    let mut links: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(ados_macpin::engine::is_pin_link_file)
        })
        .collect();
    links.sort();
    links
}

/// The per-plugin system accounts in a `/etc/passwd` body (pure).
fn plugin_users(passwd: &str) -> Vec<String> {
    passwd
        .lines()
        .filter_map(|l| l.split(':').next())
        .filter(|name| name.starts_with(ados_plugin_host::plugin_account::PLUGIN_USER_PREFIX))
        .map(str::to_string)
        .collect()
}

/// The groups a purge removes, after every account in them is gone.
fn purge_groups() -> [&'static str; 4] {
    [
        ados_protocol::ipc::PLUGIN_GROUP,
        ados_protocol::vision_rpc::VISION_READERS_GROUP,
        ados_protocol::ipc::OPERATOR_GROUP,
        "ados",
    ]
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

/// Run the uninstall, emitting progress through `sink` so the live UI shows each
/// teardown phase (the ids match [`crate::ui::UNINSTALL_GROUPS`]). `purge`
/// additionally removes `/etc/ados` (device id, pairing, config) for a
/// from-clean reinstall.
pub fn run_uninstall(purge: bool, sink: &ProgressSink) -> anyhow::Result<()> {
    // Stop + disable + remove every ados unit, its `.wants` dropin
    // dirs + `multi-user.target.wants` links, and the system dropins outside
    // /opt/ados.
    sink.step_started("stop_units");
    let units = discover_unit_files();
    remove_units(&units);
    for wants in discover_wants_dirs() {
        let _ = std::fs::remove_dir_all(&wants);
    }
    for link in discover_target_wants_links() {
        if let Err(e) = remove_path(&link) {
            tracing::warn!(link = %link.display(), error = %e, "removing target link failed");
        }
    }
    for dropin in dropin_files() {
        let _ = remove_path(Path::new(dropin));
    }
    for link in discover_mac_pin_links(Path::new(ados_macpin::engine::NETWORKD_DIR)) {
        if let Err(e) = remove_path(&link) {
            tracing::warn!(link = %link.display(), error = %e, "removing MAC-pin link failed");
        }
    }
    for unit in masked_units() {
        let _ = exec::run("systemctl", &["unmask", unit]);
    }
    // The plugin loopback guard lives in the kernel until reboot; with the
    // plugins gone it would only drop traffic for accounts removed below.
    let _ = exec::run(
        "nft",
        &[
            "delete",
            "table",
            "inet",
            ados_plugin_host::loopback_guard::TABLE,
        ],
    );
    sink.step_result("stop_units", &StepOutcome::Ok);

    // Reload systemd + udev so the removed units/rules are forgotten.
    sink.step_started("reload");
    let _ = exec::run("systemctl", &["daemon-reload"]);
    let _ = exec::run("systemctl", &["reset-failed"]);
    let _ = exec::run("udevadm", &["control", "--reload-rules"]);
    sink.step_result("reload", &StepOutcome::Ok);

    // The global `/usr/local/bin/ados*` commands.
    sink.step_started("commands");
    for link in symlinks() {
        let _ = remove_path(Path::new(link));
    }
    sink.step_result("commands", &StepOutcome::Ok);

    // The install + state + data + log + runtime trees, the MOTD, and
    // (only on --purge) the config. `/var/ados` and `/var/log/ados` are not in
    // the env path constants; the canonical removal list names them literally.
    sink.step_started("files");
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
    if purge {
        let _ = std::fs::remove_dir_all(env::CONFIG_DIR);
    }
    sink.step_result("files", &StepOutcome::Ok);

    // The per-plugin accounts own nothing once /var/ados is gone, so they go on
    // every uninstall. The agent's own account and groups go only on purge,
    // with the config they belong to.
    let passwd = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
    for user in plugin_users(&passwd) {
        let _ = exec::run("userdel", &[&user]);
    }
    if purge {
        let _ = exec::run("userdel", &["ados"]);
        for group in purge_groups() {
            let _ = exec::run("groupdel", &[group]);
        }
    }

    // Revert residue so a GS→drone flip leaves a clean box.
    sink.step_started("cleanup");
    crate::steps::purge_residue::revert_residue();
    sink.step_result("cleanup", &StepOutcome::Ok);

    tracing::info!(purge, "ADOS Drone Agent uninstalled");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uninstall_unmasks_everything_the_install_masks() {
        // Masking is a symlink to /dev/null in /etc/systemd/system. It is not a
        // file under /opt/ados and deleting the agent's drop-ins does not undo
        // it, so without an explicit unmask the box was left a headless
        // appliance that could not sleep and would not start a desktop, with
        // nothing on it left to explain why.
        //
        // Compared against the masking sites themselves rather than a second
        // hand-written list, so adding a mask over there fails here instead of
        // silently becoming permanent.
        let unmasked = masked_units();
        for unit in crate::steps::appliance::sleep_targets() {
            assert!(
                unmasked.contains(&unit),
                "uninstall must unmask {unit}, which the install masks on every profile"
            );
        }
        for unit in crate::steps::appliance::display_manager_units() {
            assert!(
                unmasked.contains(&unit),
                "uninstall must unmask {unit}, which the ground-station install masks"
            );
        }
    }

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
            "/etc/systemd/logind.conf.d/90-ados-no-idle.conf",
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
    fn symlink_set_is_the_installed_commands_and_nothing_retired() {
        let s = symlinks();
        assert!(s.contains(&"/usr/local/bin/ados"));
        assert!(s.contains(&"/usr/local/bin/ados-supervisor"));
        // The demo console script is gone, so its command is not installed.
        assert!(!s.contains(&"/usr/local/bin/ados-agent"));
    }

    #[test]
    fn only_the_engines_pin_links_are_swept() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "10-ados-mac-wlan0.link",
            "10-ados-mac-1-1.3.link",
            "50-radxa-aic8800.link",
            "10-ados-mac-notes.txt",
        ] {
            std::fs::write(dir.path().join(name), b"").unwrap();
        }
        let names: Vec<String> = discover_mac_pin_links(dir.path())
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["10-ados-mac-1-1.3.link", "10-ados-mac-wlan0.link"]
        );
    }

    #[test]
    fn only_per_plugin_accounts_are_removed_on_every_uninstall() {
        let passwd = "root:x:0:0::/root:/bin/bash\n\
                      ados:x:998:998::/nonexistent:/usr/sbin/nologin\n\
                      ados-plg-1a2b3c4d:x:997:996::/nonexistent:/usr/sbin/nologin\n\
                      operator:x:1000:1000::/home/operator:/bin/bash\n";
        assert_eq!(plugin_users(passwd), vec!["ados-plg-1a2b3c4d".to_string()]);
    }
}
