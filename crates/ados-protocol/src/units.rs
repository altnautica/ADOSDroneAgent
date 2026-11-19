//! The systemd unit table: every unit the agent ships in `data/systemd`, with
//! the facts each consumer needs about it.
//!
//! The supervisor's managed catalog, its ground-station role sets, the
//! installer's enable / start / teardown / health lists, the REST restart
//! allowlist, the armed interlock's link-dropping class and the published unit
//! reference are all derived from [`UNITS`]. A shipped unit with no row here, or
//! a row with no shipped unit file, fails the bijection test.
//!
//! Row order is meaningful: the supervisor's catalog (and so its start order
//! within a tier) and every derived installer list follow it.

mod spec;
use spec::{
    boot_oneshot, gs_unit, sensor_unit, BASE, DRONE, DRONE_VISOR, FC, FC_VISOR, GS, GS_VISOR,
};
pub use spec::{Category, Install, UnitSpec, ALL_PROFILES};

pub use crate::host_kind::{read_host_kind, HostKind};

use Category::{Core, Hardware, OnDemand};

/// Every unit in `data/systemd`, in canonical order.
pub const UNITS: &[UnitSpec] = &[
    // The top-level unit that owns the agent process tree. It has its own
    // restart route, and the start step starts it explicitly.
    UnitSpec {
        name: "ados-supervisor.service",
        install: Install::Universal,
        health_check: true,
        api_restartable: false,
        drops_link: true,
        ..BASE
    },
    // The sole command-and-control path to the flight controller. Not on the
    // FC-less profiles, which never fetch the router binary.
    UnitSpec {
        name: "ados-mavlink.service",
        category: Some(Core),
        profiles: FC_VISOR,
        drops_link: true,
        headless_keep: true,
        ..BASE
    },
    // The on-board command authority: shapes operator sticks and arbitrates
    // skills, gotos and swarm goals into guided setpoints. Drone only.
    UnitSpec {
        name: "ados-pilot.service",
        category: Some(Core),
        profiles: DRONE,
        headless_keep: true,
        ..BASE
    },
    // The swarm state bus, the same seam as the router seen from either end.
    // A drone with no fleet slot yet exits 0 and idles.
    UnitSpec {
        name: "ados-swarmbus.service",
        category: Some(Core),
        profiles: FC,
        drops_link: true,
        headless_keep: true,
        idle_ok: true,
        ..BASE
    },
    UnitSpec {
        name: "ados-api.service",
        category: Some(Core),
        ..BASE
    },
    UnitSpec {
        name: "ados-cloud.service",
        category: Some(Core),
        drops_link: true,
        ..BASE
    },
    // The durable logging store. Opt-in: the installer masks it when
    // `logging.store.enabled` is off, and the supervisor gates on the same key.
    UnitSpec {
        name: "ados-logd.service",
        category: Some(Core),
        headless_keep: true,
        idle_ok: true,
        ..BASE
    },
    // The clock and sensor-bus broker, first in the hardware tier so the bus
    // is bound before its producers; then the USB sensor hub (marker-gated)
    // and the directly attached GNSS receivers.
    sensor_unit("ados-clock.service", FC_VISOR),
    sensor_unit("ados-sensorhub.service", DRONE_VISOR),
    sensor_unit("ados-gnss.service", GS_VISOR),
    // The encode pipeline (air side on a drone, preview legs on a visor); exits
    // 0 and idles on other profiles or with video off.
    UnitSpec {
        name: "ados-video.service",
        category: Some(Hardware),
        profiles: DRONE_VISOR,
        drops_link: true,
        headless_keep: true,
        idle_ok: true,
        teardown: true,
        ..BASE
    },
    // NPU inference sidecar (RKNN); self-gates on the NPU runtime library.
    UnitSpec {
        name: "ados-vision-rknn.service",
        category: Some(Hardware),
        profiles: DRONE,
        ..BASE
    },
    // NPU inference sidecar (Hailo); `ConditionPathExists=/dev/hailo0`.
    UnitSpec {
        name: "ados-vision-hailo.service",
        category: Some(Hardware),
        profiles: DRONE,
        install: Install::Profile,
        teardown: true,
        ..BASE
    },
    // The drone-side radio TX manager.
    UnitSpec {
        name: "ados-wfb.service",
        category: Some(Hardware),
        profiles: DRONE,
        drops_link: true,
        headless_keep: true,
        teardown: true,
        ..BASE
    },
    // The onboard vision engine; exits cleanly while vision is off.
    UnitSpec {
        name: "ados-vision.service",
        category: Some(Hardware),
        profiles: DRONE_VISOR,
        ..BASE
    },
    // The native HTTP control surface, the LAN front on every profile. This
    // process serves the restart route, so it cannot restart itself.
    UnitSpec {
        name: "ados-control.service",
        category: Some(OnDemand),
        install: Install::Universal,
        start_on_install: true,
        api_restartable: false,
        headless_keep: true,
        ..BASE
    },
    // The console and its mode: taking the virtual terminal away from the
    // local session, and probing what the display can do. A personal computer
    // keeps its own console.
    UnitSpec {
        appliance_only: true,
        ..boot_oneshot("ados-fbcon-detach.service")
    },
    UnitSpec {
        appliance_only: true,
        ..boot_oneshot("ados-display-probe.service")
    },
    // Confirms a staged CSI overlay or restores the pre-overlay boot config;
    // the probation marker, not the profile, decides whether it does anything.
    boot_oneshot("ados-camera-probe.service"),
    // Forces the USB-C port into host mode. That is the box's port, not a
    // laptop's: on a personal computer it stays where the user put it.
    UnitSpec {
        appliance_only: true,
        ..boot_oneshot("ados-usb-otg-host.service")
    },
    // The native plugin host, pulled up by `WantedBy=` the supervisor.
    UnitSpec {
        name: "ados-plugin-host.service",
        install: Install::Universal,
        start_on_install: true,
        ..BASE
    },
    // GPIO-output substrate; execs /bin/true until the opt-in marker exists.
    UnitSpec {
        name: "ados-gpio.service",
        category: Some(Hardware),
        install: Install::Universal,
        idle_ok: true,
        ..BASE
    },
    // CRSF / ExpressLRS RC lane, gated on the crsf-enabled marker.
    UnitSpec {
        name: "ados-crsf.service",
        category: Some(Hardware),
        profiles: FC,
        install: Install::Universal,
        drops_link: true,
        idle_ok: true,
        ..BASE
    },
    // Config-over-radio channel, gated on the tunnel-enabled marker.
    UnitSpec {
        name: "ados-tunnel-config.service",
        category: Some(Hardware),
        profiles: FC,
        install: Install::Universal,
        idle_ok: true,
        ..BASE
    },
    // LoRa beacon and mesh lane, gated on the lora-enabled marker.
    UnitSpec {
        name: "ados-lora.service",
        category: Some(Hardware),
        profiles: FC,
        install: Install::Universal,
        idle_ok: true,
        ..BASE
    },
    // Boot re-assert of the power knobs; the power-hardening step enables it
    // once its helper script is in place.
    UnitSpec {
        name: "ados-power.service",
        self_terminating: true,
        ..BASE
    },
    // The single-node receive plane, owned by the direct role.
    UnitSpec {
        roles: &["direct"],
        health_check: true,
        drops_link: true,
        idle_ok: true,
        ..gs_unit("ados-wfb-rx.service", Some(Hardware))
    },
    // The pairing daemon every REST pairing call reaches over its socket.
    UnitSpec {
        health_check: true,
        ..gs_unit("ados-mesh-pairing.service", None)
    },
    UnitSpec {
        health_check: true,
        drops_link: true,
        ..gs_unit("ados-mediamtx-gs.service", Some(Hardware))
    },
    UnitSpec {
        profiles: GS_VISOR,
        appliance_only: true,
        ..gs_unit("ados-oled.service", Some(Hardware))
    },
    // The optional I2C status panel self-skips when no panel answers, so the
    // install enables it without kicking it.
    UnitSpec {
        profiles: GS_VISOR,
        start_on_install: false,
        appliance_only: true,
        ..gs_unit("ados-oled-i2c.service", Some(Hardware))
    },
    UnitSpec {
        profiles: GS_VISOR,
        health_check: true,
        drops_link: true,
        appliance_only: true,
        ..gs_unit("ados-hostapd.service", Some(Hardware))
    },
    UnitSpec {
        profiles: GS_VISOR,
        health_check: true,
        drops_link: true,
        appliance_only: true,
        ..gs_unit("ados-dnsmasq-gs.service", Some(Hardware))
    },
    UnitSpec {
        health_check: true,
        idle_ok: true,
        appliance_only: true,
        ..gs_unit("ados-setup-captive.service", Some(OnDemand))
    },
    // Exits 0 when the configured display is an LCD or none.
    UnitSpec {
        idle_ok: true,
        appliance_only: true,
        ..gs_unit("ados-kiosk.service", Some(Hardware))
    },
    UnitSpec {
        profiles: GS_VISOR,
        drops_link: true,
        appliance_only: true,
        ..gs_unit("ados-input.service", Some(Hardware))
    },
    // The front-panel button reader lives in-process here, so a visor runs it
    // too.
    UnitSpec {
        profiles: GS_VISOR,
        drops_link: true,
        appliance_only: true,
        ..gs_unit("ados-pic.service", Some(Hardware))
    },
    // The ados-net uplink daemon: uplinks, the Wi-Fi station (it serves the
    // Wi-Fi command socket) and the access-point config the hostapd unit
    // gates on. A visor's AP needs it, so it runs there too.
    UnitSpec {
        profiles: GS_VISOR,
        drops_link: true,
        appliance_only: true,
        ..gs_unit("ados-uplink-router.service", Some(Hardware))
    },
    // Distributed-receive units, owned by the relay and receiver roles. The
    // supervisor starts them on a role transition; nothing enables them.
    UnitSpec {
        name: "ados-batman.service",
        category: Some(Hardware),
        profiles: GS,
        roles: &["relay", "receiver"],
        drops_link: true,
        idle_ok: true,
        teardown: true,
        appliance_only: true,
        ..BASE
    },
    UnitSpec {
        name: "ados-wfb-relay.service",
        category: Some(Hardware),
        profiles: GS,
        roles: &["relay"],
        drops_link: true,
        idle_ok: true,
        appliance_only: true,
        ..BASE
    },
    UnitSpec {
        name: "ados-wfb-receiver.service",
        category: Some(Hardware),
        profiles: GS,
        roles: &["receiver"],
        drops_link: true,
        idle_ok: true,
        appliance_only: true,
        ..BASE
    },
];

/// Units a prior release shipped and has since deleted. The installer prunes
/// each on every run so an upgrade leaves no orphaned unit whose code is gone.
/// An explicit list, not a "remove what is not shipped" sweep: the runtime
/// plugin slice and the per-plugin units live outside `data/systemd`.
pub const RETIRED: &[&str] = &[
    "ados-scripting.service",
    "ados-cloud-relay.service",
    // The native ados-pic reads the front-panel buttons in-process.
    "ados-buttons.service",
    // The native net daemon owns both uplinks.
    "ados-ethernet.service",
    "ados-modem.service",
    // The native net daemon composes the USB gadget in-process.
    "ados-usb-gadget-setup.service",
    "ados-usb-gadget.service",
    // The REST API reads the peripheral registry in-process.
    "ados-peripherals.service",
    // The native control surface owns the `_ados._tcp` advert.
    "ados-discovery.service",
    // The logging store reads its own hardware snapshot.
    "ados-health.service",
    // The native net daemon owns the WiFi station.
    "ados-wifi-client.service",
];

mod lookup;
mod roles;
pub use lookup::{
    appliance_teardown_units, find, health_units, net_daemon_owns_wifi, other_profile_units,
    profile_enable_units, start_units, supervised, units_for_profile, universal_units,
};
pub use roles::{role_unit_count, role_unit_names};
