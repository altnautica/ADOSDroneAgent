//! The unit lists the install acts on, each derived from the unit table
//! (`ados_protocol::units`). Order follows the table.

use ados_protocol::units::{Install, UNITS};

/// Units a prior release shipped and has since deleted, pruned on every
/// install (`prune_retired_units`).
pub(crate) use ados_protocol::units::RETIRED as RETIRED_UNITS;

/// Udev rules retired with the units they triggered. A rule left on an
/// upgraded box fires `systemctl start` for a unit that no longer exists, so a
/// modem hot-plug logs a failure forever. Pruned on every install
/// (`prune_retired_udev_rules`).
pub(super) const RETIRED_UDEV_RULES: &[&str] = &[
    // Started ados-modem.service on a SIMCom / Qualcomm 4G modem appearing.
    "99-ados-modem.rules",
];

/// Units enable-linked on every install, whatever the profile: the
/// supervisor, the control front and the self-gating cross-profile set.
pub(crate) fn universal_units() -> Vec<&'static str> {
    UNITS
        .iter()
        .filter(|u| u.install == Install::Universal)
        .map(|u| u.name)
        .collect()
}

/// The units the install enable-links for `profile` on top of the universal
/// set (the START half is the `start` step's job).
pub(crate) fn profile_enable_units(profile: &str) -> Vec<&'static str> {
    UNITS
        .iter()
        .filter(|u| u.install == Install::Profile && u.profiles.contains(&profile))
        .map(|u| u.name)
        .collect()
}

/// The other-profile teardown list for an install of `profile`
/// (`disable_other_profile_units`): a ground station stops the air-side units,
/// any other profile stops the ground-station units.
pub(crate) fn other_profile_units(profile: &str) -> Vec<&'static str> {
    UNITS
        .iter()
        .filter(|u| u.teardown && !u.on_profile(profile))
        .map(|u| u.name)
        .collect()
}

/// The units the `start` step kicks with `--no-block` on `profile`.
pub(crate) fn start_units(profile: &str) -> Vec<&'static str> {
    UNITS
        .iter()
        .filter(|u| u.start_on_install && u.on_profile(profile))
        .map(|u| u.name)
        .collect()
}

/// The units the health gate requires enabled on `profile`. Every one is also
/// enable-linked by this step, so completeness never demands a unit the
/// installer does not create.
pub(crate) fn health_units(profile: &str) -> Vec<&'static str> {
    UNITS
        .iter()
        .filter(|u| u.health_check && u.on_profile(profile))
        .map(|u| u.name)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted(mut v: Vec<&'static str>) -> Vec<&'static str> {
        v.sort_unstable();
        v
    }

    /// The lists as they were hand-written before the unit table, so a table
    /// edit that changes what an install does is a visible test change.
    #[test]
    fn installer_lists_unchanged() {
        assert_eq!(
            universal_units(),
            [
                // Formerly enabled by their own calls, ahead of the list.
                "ados-supervisor.service",
                "ados-control.service",
                "ados-fbcon-detach.service",
                "ados-display-probe.service",
                "ados-camera-probe.service",
                "ados-usb-otg-host.service",
                "ados-plugin-host.service",
                "ados-gpio.service",
                "ados-crsf.service",
                "ados-tunnel-config.service",
                "ados-lora.service",
            ]
        );
        const GS_ENABLE: [&str; 12] = [
            "ados-wfb-rx.service",
            "ados-mesh-pairing.service",
            "ados-mediamtx-gs.service",
            "ados-oled.service",
            "ados-oled-i2c.service",
            "ados-hostapd.service",
            "ados-dnsmasq-gs.service",
            "ados-setup-captive.service",
            "ados-kiosk.service",
            "ados-input.service",
            "ados-pic.service",
            "ados-uplink-router.service",
        ];
        assert_eq!(profile_enable_units("ground_station"), GS_ENABLE);
        assert_eq!(profile_enable_units("drone"), ["ados-vision-hailo.service"]);
        assert!(profile_enable_units("workstation").is_empty());
        assert!(profile_enable_units("compute").is_empty());

        assert_eq!(
            sorted(other_profile_units("ground_station")),
            [
                "ados-sensorhub.service",
                "ados-video.service",
                "ados-vision-hailo.service",
                "ados-wfb.service",
            ]
        );
        let gs_teardown = sorted(vec![
            "ados-wfb-rx.service",
            "ados-mediamtx-gs.service",
            "ados-oled.service",
            "ados-oled-i2c.service",
            "ados-hostapd.service",
            "ados-dnsmasq-gs.service",
            "ados-setup-captive.service",
            "ados-kiosk.service",
            "ados-input.service",
            "ados-pic.service",
            "ados-uplink-router.service",
            "ados-batman.service",
            "ados-mesh-pairing.service",
            "ados-gnss.service",
        ]);
        assert_eq!(sorted(other_profile_units("drone")), gs_teardown);
        // A workstation or compute node also stops the air-side units and the
        // sensor plane: none of them runs off a drone, ground station or visor.
        for profile in ["workstation", "compute"] {
            let mut expected = gs_teardown.clone();
            expected.extend([
                "ados-video.service",
                "ados-vision-hailo.service",
                "ados-wfb.service",
                "ados-clock.service",
                "ados-sensorhub.service",
            ]);
            assert_eq!(sorted(other_profile_units(profile)), sorted(expected));
        }

        // The start set: the control front and plugin host everywhere, plus
        // the ground-station set minus the I2C panel (enabled, never kicked).
        assert_eq!(
            start_units("drone"),
            ["ados-control.service", "ados-plugin-host.service"]
        );
        let gs_start: Vec<&str> = ["ados-control.service", "ados-plugin-host.service"]
            .into_iter()
            .chain(
                GS_ENABLE
                    .into_iter()
                    .filter(|u| *u != "ados-oled-i2c.service"),
            )
            .collect();
        assert_eq!(start_units("ground_station"), gs_start);

        assert_eq!(health_units("drone"), ["ados-supervisor.service"]);
        assert_eq!(
            health_units("ground_station"),
            [
                "ados-supervisor.service",
                "ados-wfb-rx.service",
                "ados-mesh-pairing.service",
                "ados-mediamtx-gs.service",
                "ados-hostapd.service",
                "ados-dnsmasq-gs.service",
                "ados-setup-captive.service",
            ]
        );
    }

    /// A visor runs the access point (and the ados-net daemon that renders
    /// it), the panel and the buttons, keeps its
    /// camera pipeline, and tears down every radio, receive and kiosk unit.
    #[test]
    fn visor_lists() {
        assert_eq!(
            profile_enable_units("visor"),
            [
                "ados-oled.service",
                "ados-oled-i2c.service",
                "ados-hostapd.service",
                "ados-dnsmasq-gs.service",
                "ados-input.service",
                "ados-pic.service",
                "ados-uplink-router.service",
            ]
        );
        assert_eq!(
            sorted(other_profile_units("visor")),
            sorted(vec![
                "ados-wfb.service",
                "ados-vision-hailo.service",
                "ados-wfb-rx.service",
                "ados-mesh-pairing.service",
                "ados-mediamtx-gs.service",
                "ados-setup-captive.service",
                "ados-kiosk.service",
                "ados-batman.service",
            ])
        );
        assert_eq!(
            start_units("visor"),
            [
                "ados-control.service",
                "ados-plugin-host.service",
                "ados-oled.service",
                "ados-hostapd.service",
                "ados-dnsmasq-gs.service",
                "ados-input.service",
                "ados-pic.service",
                "ados-uplink-router.service",
            ]
        );
        assert_eq!(
            health_units("visor"),
            [
                "ados-supervisor.service",
                "ados-hostapd.service",
                "ados-dnsmasq-gs.service",
            ]
        );
    }

    #[test]
    fn retired_units_are_never_enabled_started_or_expected() {
        for profile in ["drone", "ground_station", "workstation", "compute", "visor"] {
            for unit in RETIRED_UNITS {
                assert!(!universal_units().contains(unit), "{unit}");
                assert!(!profile_enable_units(profile).contains(unit), "{unit}");
                assert!(!start_units(profile).contains(unit), "{unit}");
                assert!(!health_units(profile).contains(unit), "{unit}");
            }
        }
    }

    #[test]
    fn every_started_or_expected_unit_is_enabled() {
        // A unit started or health-checked but not enabled would not come
        // back after a reboot.
        for profile in ["drone", "ground_station", "workstation", "compute", "visor"] {
            let mut enabled = universal_units();
            enabled.extend(profile_enable_units(profile));
            for unit in start_units(profile).iter().chain(&health_units(profile)) {
                assert!(enabled.contains(unit), "{unit} on {profile}");
            }
        }
    }

    #[test]
    fn gs_enable_list_is_torn_down_on_a_drone() {
        let drone_teardown = other_profile_units("drone");
        for unit in profile_enable_units("ground_station") {
            assert!(drone_teardown.contains(&unit), "{unit}");
        }
        // Never the supervisor or a cross-profile unit.
        assert!(!drone_teardown.contains(&"ados-supervisor.service"));
        assert!(!drone_teardown.contains(&"ados-plugin-host.service"));
    }
}
