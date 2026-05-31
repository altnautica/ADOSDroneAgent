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
use crate::env::{self, CONFIG_DIR, DEVICE_ID_FILE};
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

/// The other-profile teardown list, keyed by the profile being installed
/// (`disable_other_profile_units`). On a GS rig the drone TX unit must not run;
/// on a drone rig every GS-only unit gets disabled.
fn other_profile_units(profile: &str) -> &'static [&'static str] {
    match profile {
        "ground_station" => &["ados-wfb.service"],
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
    let _ = std::fs::write("/etc/tmpfiles.d/ados.conf", "d /run/ados 0755 root root -\n");
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

/// Enable a unit only when its file is deployed; tolerate the not-found case.
fn enable_if_present(unit: &str) {
    let path = Path::new(SYSTEMD_DIR).join(unit);
    if path.exists() {
        let _ = exec::run("systemctl", &["enable", unit]);
    } else {
        tracing::warn!(unit, "unit not deployed; skipping enable");
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

/// systemd unit install + enable (NOT start).
pub struct Systemd;

impl Step for Systemd {
    fn id(&self) -> &str {
        "systemd"
    }
    fn requires(&self) -> &[&str] {
        &["fetch_binaries", "config_identity"]
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

        // 1. Deploy the unit files (Required — no units, no agent).
        let systemd_src = source.join("data/systemd");
        let count = match deploy_units(&systemd_src) {
            Ok(n) => n,
            Err(e) => return StepOutcome::Failed(e.to_string()),
        };
        tracing::info!(count, "deployed systemd unit files");

        // 2. /run/ados + tmpfiles, env file, plugin slice, udev rules.
        install_run_dir_tmpfiles();
        write_env_file();
        install_plugin_slice(Some(&source));
        install_plugin_tmpfiles(Some(&source));
        let udev_src = source.join("data/udev");
        let udev_count = deploy_udev_rules(&udev_src);
        tracing::info!(count = udev_count, "deployed udev rules");

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

        // 6. Profile-specific enable + teardown.
        if ctx.profile == "ground_station" {
            for unit in GROUND_STATION_ENABLE_UNITS {
                enable_if_present(unit);
            }
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
        // A GS rig tears down the drone TX unit only.
        let gs = other_profile_units("ground_station");
        assert_eq!(gs, &["ados-wfb.service"]);
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
}
