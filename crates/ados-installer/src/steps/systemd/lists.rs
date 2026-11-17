//! The unit lists the install acts on. Each lives in the unit table's crate
//! (`ados_protocol::units`), shared with the runtime profile switch
//! (`ados-supervisor switch-profile`); order follows the table.
//!
//! - `universal_units`: enable-linked on every install;
//! - `profile_enable_units`: enable-linked for the profile (the START half is
//!   the `start` step's job);
//! - `other_profile_units`: the other profile's units, disabled by the systemd
//!   step (`disable_other_profile_units`) and stopped by the start step with
//!   the supervisor down (`cycle_supervisor`);
//! - `start_units`: kicked by the `start` step with `--no-block`;
//! - `health_units`: required enabled by the health gate. Every one is also
//!   enable-linked by the systemd step, so completeness never demands a unit
//!   the installer does not create.

pub(crate) use ados_protocol::units::{
    health_units, other_profile_units, profile_enable_units, start_units, universal_units,
};

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

#[cfg(test)]
mod tests {
    use super::*;

    /// The lists as every shipped board runs them. A personal computer's lists
    /// have their own cases below; an edit that changes an appliance install
    /// shows up here.
    const HOST: ados_protocol::host_kind::HostKind = ados_protocol::host_kind::HostKind::Appliance;

    fn sorted(mut v: Vec<&'static str>) -> Vec<&'static str> {
        v.sort_unstable();
        v
    }

    /// The lists as they were hand-written before the unit table, so a table
    /// edit that changes what an install does is a visible test change.
    #[test]
    fn installer_lists_unchanged() {
        assert_eq!(
            universal_units(HOST),
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
        assert_eq!(profile_enable_units("ground_station", HOST), GS_ENABLE);
        assert_eq!(
            profile_enable_units("drone", HOST),
            ["ados-vision-hailo.service"]
        );
        assert!(profile_enable_units("workstation", HOST).is_empty());
        assert!(profile_enable_units("compute", HOST).is_empty());

        assert_eq!(
            sorted(other_profile_units("ground_station", HOST)),
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
        assert_eq!(sorted(other_profile_units("drone", HOST)), gs_teardown);
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
            assert_eq!(sorted(other_profile_units(profile, HOST)), sorted(expected));
        }

        // The start set: the control front and plugin host everywhere, plus
        // the ground-station set minus the I2C panel (enabled, never kicked).
        assert_eq!(
            start_units("drone", HOST),
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
        assert_eq!(start_units("ground_station", HOST), gs_start);

        assert_eq!(health_units("drone", HOST), ["ados-supervisor.service"]);
        assert_eq!(
            health_units("ground_station", HOST),
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
            profile_enable_units("visor", HOST),
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
            sorted(other_profile_units("visor", HOST)),
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
            start_units("visor", HOST),
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
            health_units("visor", HOST),
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
                assert!(!universal_units(HOST).contains(unit), "{unit}");
                assert!(
                    !profile_enable_units(profile, HOST).contains(unit),
                    "{unit}"
                );
                assert!(!start_units(profile, HOST).contains(unit), "{unit}");
                assert!(!health_units(profile, HOST).contains(unit), "{unit}");
            }
        }
    }

    #[test]
    fn every_started_or_expected_unit_is_enabled() {
        // A unit started or health-checked but not enabled would not come
        // back after a reboot.
        for host in [
            ados_protocol::host_kind::HostKind::Appliance,
            ados_protocol::host_kind::HostKind::Personal,
        ] {
            for profile in ["drone", "ground_station", "workstation", "compute", "visor"] {
                let mut enabled = universal_units(host);
                enabled.extend(profile_enable_units(profile, host));
                for unit in start_units(profile, host)
                    .iter()
                    .chain(&health_units(profile, host))
                {
                    assert!(enabled.contains(unit), "{unit} on {profile} on {host}");
                }
            }
        }
    }

    #[test]
    fn gs_enable_list_is_torn_down_on_a_drone() {
        let drone_teardown = other_profile_units("drone", HOST);
        for unit in profile_enable_units("ground_station", HOST) {
            assert!(drone_teardown.contains(&unit), "{unit}");
        }
        // Never the supervisor or a cross-profile unit.
        assert!(!drone_teardown.contains(&"ados-supervisor.service"));
        assert!(!drone_teardown.contains(&"ados-plugin-host.service"));
    }

    /// A ground station installed on somebody's own computer keeps the radio,
    /// pairing, video and control services and nothing that would take the
    /// console, the display, the Wi-Fi station, the routing table or the USB
    /// port's mode away from them.
    #[test]
    fn a_personal_ground_station_enables_only_the_link_services() {
        use ados_protocol::host_kind::HostKind;
        let personal = HostKind::Personal;
        assert_eq!(
            profile_enable_units("ground_station", personal),
            [
                "ados-wfb-rx.service",
                "ados-mesh-pairing.service",
                "ados-mediamtx-gs.service",
            ]
        );
        assert_eq!(
            health_units("ground_station", personal),
            [
                "ados-supervisor.service",
                "ados-wfb-rx.service",
                "ados-mesh-pairing.service",
                "ados-mediamtx-gs.service",
            ]
        );
        // Nothing appliance-only survives in any list a personal install acts
        // on, at any profile.
        for profile in ["drone", "ground_station", "workstation", "compute", "visor"] {
            for unit in profile_enable_units(profile, personal)
                .into_iter()
                .chain(universal_units(personal))
                .chain(start_units(profile, personal))
            {
                assert!(
                    !ados_protocol::units::find(unit).is_some_and(|u| u.appliance_only),
                    "{unit} is appliance-only but enabled on {profile}"
                );
            }
        }
    }

    /// A host that ran the appliance set and is now somebody's computer stops
    /// and disables every appliance-only unit it was carrying, rather than
    /// merely leaving them out of the new lists.
    #[test]
    fn a_host_flip_to_personal_tears_the_appliance_units_down() {
        use ados_protocol::host_kind::HostKind;
        let torn = other_profile_units("ground_station", HostKind::Personal);
        for unit in [
            "ados-hostapd.service",
            "ados-dnsmasq-gs.service",
            "ados-kiosk.service",
            "ados-oled.service",
            "ados-oled-i2c.service",
            "ados-setup-captive.service",
            "ados-input.service",
            "ados-pic.service",
            "ados-uplink-router.service",
        ] {
            assert!(torn.contains(&unit), "{unit} not torn down");
        }
        // The link services stay.
        for unit in [
            "ados-wfb-rx.service",
            "ados-mesh-pairing.service",
            "ados-mediamtx-gs.service",
        ] {
            assert!(!torn.contains(&unit), "{unit} must not be torn down");
        }
        // The same list on a drone covers the ground-station set it was never
        // running too, as before.
        let drone = other_profile_units("drone", HostKind::Personal);
        assert!(drone.contains(&"ados-hostapd.service"));
        // An appliance tears down nothing extra.
        assert_eq!(
            ados_protocol::units::appliance_teardown_units("ground_station", HostKind::Appliance),
            Vec::<&str>::new()
        );
        assert!(ados_protocol::units::appliance_teardown_units(
            "ground_station",
            HostKind::Personal
        )
        .contains(&"ados-hostapd.service"));
    }
}
