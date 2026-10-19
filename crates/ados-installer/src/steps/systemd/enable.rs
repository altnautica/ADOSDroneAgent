//! Deploying the unit files, udev rules and tmpfiles drop-ins, provisioning
//! the system identity and groups, and enable-linking a deployed unit.

use super::*;

/// Deploy every `ados-*.{service,slice,target,timer}` unit from the source
/// `data/systemd` dir to `/etc/systemd/system`, rewriting the venv path. Returns
/// the count deployed, or an error when the source dir is absent (Required: no
/// units means no agent).
pub(super) fn deploy_units(systemd_src: &Path) -> anyhow::Result<usize> {
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
pub(super) fn deploy_udev_rules(udev_src: &Path) -> usize {
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
        // Apply the freshly-deployed hot-plug rules to devices already present,
        // but scope the trigger AWAY from the `net` subsystem: a bare trigger
        // re-runs net_setup_link on the live management interface (e.g. wlan0),
        // which re-applies any pinned MAC and drops the operator's link mid-run.
        // (Same hazard `install_power_hardening` avoids by scoping to usb.) The
        // net-targeting rule here — WiFi power-save off — is already re-asserted
        // by the ados-power boot oneshot and the supervisor's runtime backstop.
        let _ = exec::run("udevadm", &["trigger", "--subsystem-nomatch=net"]);
    }
    count
}

/// Write `contents` to `path` atomically (a temp file in the same directory,
/// then rename). A process killed mid-write then leaves either the old file or
/// the complete new one — never a truncated/0-byte file. The front-cutover
/// drop-ins use this so an interrupted install can never strand the control
/// surface on the wrong LAN port with an empty `ADOS_CONTROL_PORT`.
pub(super) fn write_atomic(path: &Path, contents: impl AsRef<[u8]>) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

/// Create `/run/ados` (+ tmpfiles rule) so the Unix sockets have a home that
/// survives reboot. Idempotent.
pub(super) fn install_run_dir_tmpfiles() {
    const DEST: &str = "/etc/tmpfiles.d/ados.conf";
    let _ = std::fs::create_dir_all("/run/ados");
    if std::fs::write(DEST, run_dir_tmpfiles_body()).is_ok() {
        let _ = exec::run("systemd-tmpfiles", &["--create", DEST]);
    }
}

/// The run-dir tmpfiles body. `/run/ados/capture` is the capture-session
/// state dir: setgid and writable by [`ados_protocol::ipc::CAPTURE_GROUP`]
/// only, so a sandboxed capture plugin (which joins that group with
/// `capture.storage.write` and gets the dir bound into its sandbox) can create
/// and remove the `active` marker the MAVLink router reads to pick its capture
/// stream rates, without write access to the rest of `/run/ados` and without
/// the operator group that guards the command sockets.
pub(super) fn run_dir_tmpfiles_body() -> String {
    format!(
        "d /run/ados 0755 root root -\nd /run/ados/capture 2775 root {} -\n",
        ados_protocol::ipc::CAPTURE_GROUP
    )
}

/// Write the `/etc/ados/env` file from the persisted device id.
pub(super) fn write_env_file() {
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

/// Install the plugin runtime tmpfiles drop-in so `/run/ados/plugins` (the per-plugin
/// socket dir) is recreated on every boot, not just at install time.
pub(super) fn install_plugin_tmpfiles(source: Option<&Path>) {
    const DEST: &str = "/etc/tmpfiles.d/ados-plugins.conf";
    let inline = "# ADOS plugin runtime sockets and runtime state\n\
d /run/ados/plugins 0750 ados ados -\n\
r! /run/ados/plugins/*/host.sock\n";

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

/// Install the vision framebus tmpfiles drop-in so a stale `/dev/shm` frame
/// ring is swept at boot.
///
/// The ring is unlinked by its writer's `Drop`, which does not run on SIGKILL
/// or an OOM kill — and the file is named after the camera's `/dev/videoN`
/// index, which re-enumerates on USB hot-plug, so each new node number stranded
/// the previous run's ring in RAM permanently. Shipped in the tree but not
/// installed by name is the same as not shipped: the install must place it.
pub(super) fn install_vision_tmpfiles(source: Option<&Path>) {
    const DEST: &str = "/etc/tmpfiles.d/ados-vision.conf";
    let Some(src) = source.map(|s| s.join("etc/tmpfiles.d/ados-vision.conf")) else {
        return;
    };
    if !src.is_file() {
        tracing::warn!(path = %src.display(), "vision tmpfiles drop-in missing from the source tree");
        return;
    }
    if let Err(e) = std::fs::copy(&src, DEST) {
        tracing::warn!(error = %e, "installing the vision tmpfiles drop-in failed");
        return;
    }
    set_mode(Path::new(DEST), 0o644);
    let _ = exec::run("systemd-tmpfiles", &["--create", DEST]);
}

/// Install the plugin cgroup slice: delegate to `scripts/setup-plugin-slice.sh`
/// when present (it runs its own daemon-reload), else write the slice inline.
pub(super) fn install_plugin_slice(source: Option<&Path>) {
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
pub(super) fn install_logd_store_dir() {
    const DIR: &str = "/var/ados/logd";
    if let Err(e) = std::fs::create_dir_all(DIR) {
        tracing::warn!(error = %e, dir = DIR, "creating logging store dir failed");
        return;
    }
    set_mode(Path::new(DIR), 0o750);
}

/// Enable a unit only when its file is deployed; tolerate the not-found case.
pub(super) fn enable_if_present(unit: &str) {
    let path = Path::new(SYSTEMD_DIR).join(unit);
    if path.exists() {
        let _ = exec::run("systemctl", &["enable", unit]);
    } else {
        tracing::warn!(unit, "unit not deployed; skipping enable");
    }
}

/// Provision the `ados` system user and group plus the plugin groups
/// (idempotent). The plugin runtime dir is created `0750 ados ados`
/// (`install_plugin_tmpfiles`) and the ground-station hardware-group
/// memberships run `usermod -aG <grp> ados`. Every plugin runs as its own
/// `ados-plg-*` user whose primary group is [`ados_protocol::ipc::PLUGIN_GROUP`];
/// a plugin granted camera frames also carries
/// [`ados_protocol::vision_rpc::VISION_READERS_GROUP`], one granted capture
/// storage [`ados_protocol::ipc::CAPTURE_GROUP`], and one granted the sensor
/// bus [`ados_protocol::ipc::SENSORS_GROUP`]. systemd refuses a unit whose
/// `Group=`/`SupplementaryGroups=` names a missing group, so all exist before
/// any plugin is installed.
///
/// A system account: no login shell, no home directory, allocated below the
/// regular-uid range. The group and the user are each created only when absent,
/// mirroring the getent/id idempotency the rest of this module uses.
pub(super) fn provision_ados_identity() {
    if !exec::run_ok("getent", &["group", "ados"]) {
        let _ = exec::run("groupadd", &["--system", "ados"]);
    }
    // The group that owns the agent's command sockets. The `ados` plugin user
    // is never added to it, so a plugin cannot open them.
    if !exec::run_ok("getent", &["group", ados_protocol::ipc::OPERATOR_GROUP]) {
        let _ = exec::run(
            "groupadd",
            &["--system", ados_protocol::ipc::OPERATOR_GROUP],
        );
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
    for grp in [
        ados_protocol::ipc::PLUGIN_GROUP,
        ados_protocol::vision_rpc::VISION_READERS_GROUP,
        ados_protocol::ipc::CAPTURE_GROUP,
        ados_protocol::ipc::SENSORS_GROUP,
    ] {
        if !exec::run_ok("getent", &["group", grp]) {
            let _ = exec::run("groupadd", &["--system", grp]);
        }
    }
}

/// Where the plugin capability-token secret lives on a Linux node.
pub(super) const PLUGIN_TOKEN_SECRET: &str = "/etc/ados/secrets/plugin-token-secret";

/// Create the plugin capability-token secret once, before any service that
/// signs or checks a token starts, so no two processes race to create it.
/// An existing secret is kept.
pub(super) fn provision_plugin_token_secret() {
    if let Err(e) =
        ados_plugin_host::token_secret::load_or_create_secret(Path::new(PLUGIN_TOKEN_SECRET))
    {
        tracing::warn!(error = %e, path = PLUGIN_TOKEN_SECRET, "plugin token secret could not be provisioned");
    }
}

/// The hardware-peripheral groups that ADOS systemd units name via
/// `SupplementaryGroups=` (today `gpio`; `i2c` and `spi` back the ground-station
/// OLED and future peripheral units, and the GS group memberships). Raspberry Pi
/// OS and the Radxa BSP create these via their board packages/udev, but a generic
/// Armbian image does not — and systemd fails a unit BEFORE `ExecStart` when a
/// `SupplementaryGroups=` entry names a group that does not exist (exit
/// 216/GROUP). Provisioning them lets those units start on any image; device
/// access is separate (the consumers run as root, so they need only the group to
/// exist for systemd to resolve the directive).
pub(super) const ADOS_HARDWARE_GROUPS: &[&str] = &["gpio", "i2c", "spi"];

/// Create the hardware-peripheral groups if absent (idempotent), mirroring the
/// `ados` group creation in `provision_ados_identity`. A no-op on images that
/// already ship them (Raspberry Pi OS / Radxa BSP). Must run before any unit is
/// enabled or started so a unit's `SupplementaryGroups=` resolves.
pub(super) fn provision_hardware_groups() {
    for grp in ADOS_HARDWARE_GROUPS {
        if !exec::run_ok("getent", &["group", grp]) {
            let _ = exec::run("groupadd", &["--system", grp]);
        }
    }
}

/// Add the human operator (the user who ran the install under sudo) to the `ados`
/// group, so on-box non-root RCA works without sudo: the logging store's query
/// socket is group-owned by `ados` at mode 0o660, so a group member can run
/// `ados logs query` directly. Idempotent; a no-op when there is no sudo operator
/// (the install ran directly as root) or the operator is root. The membership
/// takes effect on the operator's next login; a current session still uses sudo
/// until it reconnects.
pub(super) fn add_operator_to_ados_group() {
    let operator = match std::env::var("SUDO_USER") {
        Ok(user) if !user.is_empty() && user != "root" => user,
        _ => return,
    };
    if !exec::run_ok("id", &[operator.as_str()]) {
        return;
    }
    // `ados` for the plugin/log file plane, the operator group for the
    // command sockets (control, logd query, CLI) that plugins must not reach,
    // and the sensor bus reader group, which owns the bus socket file.
    for group in [
        "ados",
        ados_protocol::ipc::OPERATOR_GROUP,
        ados_protocol::ipc::SENSORS_GROUP,
    ] {
        if exec::run_ok("getent", &["group", group]) {
            let _ = exec::run("usermod", &["-aG", group, operator.as_str()]);
        }
    }
}

/// Add the `ados` and `pi` service users to the hardware-access groups the
/// access-point and panel services need (ground station and visor). Ports the
/// `usermod -aG ...` bits of `enable_ground_station_units`:
///
/// - `gpio`    — button service drives /dev/gpiochip0 via libgpiod.
/// - `input`   — input manager + PIC arbiter read /dev/input (gamepads/evdev).
/// - `plugdev` — USB device access for hot-plugged peripherals.
/// - `bluetooth` — D-Bus access for the PIC arbiter.
/// - `i2c`     — userspace OLED + future I2C peripherals.
///
/// Every `usermod -aG` is an idempotent no-op when membership already exists.
pub(super) fn add_panel_group_memberships() {
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
