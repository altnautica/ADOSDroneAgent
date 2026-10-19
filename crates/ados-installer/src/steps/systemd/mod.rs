//! systemd: deploy the service units + udev rules, write the env file, install
//! the plugin slice, `daemon-reload`, and ENABLE the right units for the
//! profile. Required. Checkpoint `systemd`. Runs only after both the binaries
//! are present and the config/identity exists.
//!
//! Ports the deploy + enable portions of `scripts/install.d/07-systemd.sh`
//! (`install_systemd_service`, `enable_universal_units`,
//! `disable_other_profile_units`, `enable_ground_station_units` ENABLE half,
//! `mask_conflicting_standalone_services`).
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
use crate::steps::boot_config;

/// Where unit files + dropins are deployed.
const SYSTEMD_DIR: &str = "/etc/systemd/system";
/// The udev rules dir.
const UDEV_RULES_DIR: &str = "/etc/udev/rules.d";

mod capture_root;
mod enable;
mod hardening;
mod lists;
mod reconcile;
mod render;
mod teardown;

pub use capture_root::*;
use enable::*;
pub use hardening::*;
pub(crate) use lists::*;
pub use reconcile::*;
pub use render::*;
use teardown::*;

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

        // System identity: provision the `ados` user + group and the plugin
        // groups (idempotent) before anything that consumes them, so the
        // tmpfiles chown, the usermod and every plugin unit's group resolve
        // instead of silently no-opping. The plugin token secret is created
        // here, once, before the services that read it start.
        provision_ados_identity();
        provision_plugin_token_secret();

        // Create the hardware-peripheral groups (gpio/i2c/spi) when the base
        // image lacks them, before any unit is enabled or started, so a unit's
        // `SupplementaryGroups=` resolves instead of aborting the unit at
        // 216/GROUP (the generic-Armbian case — the BSP/Pi images already ship
        // them, where this is a no-op).
        provision_hardware_groups();

        // Let the human operator read the logging store's query socket without
        // sudo: the socket is group-owned by `ados` at 0o660, so add the sudo
        // operator to the group. Idempotent; effective on their next login.
        add_operator_to_ados_group();

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
        install_vision_tmpfiles(Some(&source));
        install_logd_store_dir();
        let udev_src = source.join("data/udev");
        let udev_count = deploy_udev_rules(&udev_src);
        tracing::info!(count = udev_count, "deployed udev rules");
        // 2a. Prune udev rules retired with the units they triggered, AFTER the
        //     deploy (which copies whatever the source dir holds) so a rule the
        //     source no longer ships cannot survive an upgrade and keep firing
        //     `systemctl start` at a unit that is gone.
        prune_retired_udev_rules();

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

        // 4. Cross-profile units, the supervisor and the control front among
        //    them (enabled, NOT started: that is the `start` step's job, gated
        //    on fetch_binaries so the binary is guaranteed present).
        for unit in universal_units() {
            enable_if_present(unit);
        }
        let _ = std::fs::create_dir_all("/etc/ados/peripherals");

        // 5a-bis. Mirror `radio.crsf.enabled` onto the marker the ados-crsf
        //     unit's ConditionPathExists gates on, so a config that opts the
        //     RC lane in has the unit up after this install with no manual
        //     systemctl (and an opted-out config leaves it cleanly skipped).
        reconcile_crsf_marker();
        // Same idiom for an explicit `sensors.hub.transport`: the
        //     ados-sensorhub unit gates on `sensorhub-enabled` or on the USB
        //     presence marker the board probe writes.
        reconcile_sensorhub_marker();

        // 5a-ter. Mirror `radio.tunnel.enabled` onto the marker the
        //     ados-tunnel-config unit's ConditionPathExists gates on, the same
        //     idiom: an opted-in config has the unit up after this install with
        //     no manual systemctl, an opted-out config leaves it skipped.
        reconcile_tunnel_marker();

        // 5a-quater. Mirror `network.hotspot.enabled` onto the marker the
        //     ados-dnsmasq-gs unit's ConditionPathExists gates on. hostapd
        //     idles-in-place rather than exiting when the operator has not
        //     opted in, so its unit stays active and cannot itself gate the
        //     DHCP/DNS unit; without this marker dnsmasq starts anyway and
        //     fails forever trying to bind an interface that is serving as a
        //     WiFi client.
        reconcile_hotspot_marker();

        // 5a-quinquies. Mirror `hardware.gpio_out_enabled` onto the marker the
        //     ados-gpio unit's ConditionPathExists gates on (without it the
        //     service execs /bin/true). Default on, because the beeper and
        //     status lines are shipped behaviour, not an opt-in.
        reconcile_gpio_rust_marker();

        // 5b. The logging and telemetry store follows `logging.store.enabled`
        //     (off by default); the start step brings it up after the
        //     supervisor when it is on.
        reconcile_logd_unit();

        // 5b-bis. The native control surface is the LAN front on every node
        //     (enabled with the cross-profile set above): bind it to the LAN
        //     port and keep the residual API on the internal socket behind it.
        reconcile_front_unit();

        // 5b-ter. Grant the supervisor write access to an explicit capture
        //     root outside the paths its unit already covers (drone, visor).
        reconcile_capture_root_dropin(&ctx.profile);

        // 6. Profile-specific enable + teardown.
        for unit in profile_enable_units(&ctx.profile) {
            enable_if_present(unit);
        }
        if matches!(ctx.profile.as_str(), "ground_station" | "visor") {
            // The hardware-access group memberships belong to the AP +
            // button/joystick/OLED service set a ground station and a visor
            // share; so does the standalone hostapd/dnsmasq conflict.
            add_panel_group_memberships();
            mask_conflicting_standalone_services();
        }
        // The retired static avahi record, on every profile: an upgraded
        // ground station would otherwise keep advertising itself twice.
        remove_avahi_gs_ap();
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
mod hardening_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod unit_file_tests;
