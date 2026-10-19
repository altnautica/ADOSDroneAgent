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

/// Supervisor tier. Start order is core first, then hardware, then on-demand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// Always running on the profiles the unit applies to.
    Core,
    /// Started when its hardware or opt-in is present.
    Hardware,
    /// Brought up by its own enablement; the supervisor adopts it.
    OnDemand,
}

impl Category {
    /// The lowercase wire string used in the unit reference.
    pub const fn as_str(self) -> &'static str {
        match self {
            Category::Core => "core",
            Category::Hardware => "hardware",
            Category::OnDemand => "on_demand",
        }
    }
}

/// How the installer enable-links a unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Install {
    /// Enabled on every install, whatever the profile. Each such unit
    /// self-gates (a `ConditionPathExists`, or an `ExecStart` guard), so the
    /// link is a clean no-op on a board without its hardware.
    Universal,
    /// Enabled on an install of one of the unit's `profiles`.
    Profile,
    /// Not enable-linked from the table: the supervisor starts it, or a
    /// dedicated install step owns its enablement (the opt-in logging store,
    /// the power re-assert oneshot).
    None,
}

impl Install {
    /// The lowercase wire string used in the unit reference.
    pub const fn as_str(self) -> &'static str {
        match self {
            Install::Universal => "universal",
            Install::Profile => "profile",
            Install::None => "none",
        }
    }
}

/// One shipped systemd unit.
#[derive(Debug, Clone, Copy)]
pub struct UnitSpec {
    /// The unit file name, `ados-<name>.service`.
    pub name: &'static str,
    /// The supervisor tier, or `None` for a unit the supervisor does not
    /// manage (the supervisor itself, boot oneshots, units pulled up by
    /// `WantedBy=`).
    pub category: Option<Category>,
    /// The agent profiles the unit applies to. Empty means every profile.
    pub profiles: &'static [&'static str],
    /// The ground-station roles that own the unit. Empty means any role.
    pub roles: &'static [&'static str],
    /// How the installer enable-links it.
    pub install: Install,
    /// The install's `start` step kicks it with `--no-block` on its profiles.
    pub start_on_install: bool,
    /// The install's health gate requires it enabled on its profiles.
    pub health_check: bool,
    /// `POST /api/services/{name}/restart` may restart it.
    pub api_restartable: bool,
    /// Restarting it drops the command, telemetry, video, RC, mesh,
    /// access-point or cloud link, so the armed interlock refuses it.
    pub drops_link: bool,
    /// In the lean headless keep set: the minimal services a zero-Python
    /// flight node keeps running.
    pub headless_keep: bool,
    /// A clean `exit 0` means "nothing to run on this node", not a death (the
    /// unit file carries `RestartPreventExitStatus=0`).
    pub idle_ok: bool,
    /// A `Type=oneshot` unit that exits on its own once its work is done.
    pub self_terminating: bool,
    /// An install of a profile outside `profiles` stops and disables it.
    pub teardown: bool,
}

impl UnitSpec {
    /// The unit name without the `.service` suffix (`ados-video`).
    pub fn stem(&self) -> &'static str {
        let name = self.name;
        name.strip_suffix(".service").unwrap_or(name)
    }

    /// Whether the unit applies to `profile` (underscore form).
    pub fn on_profile(&self, profile: &str) -> bool {
        self.profiles.is_empty() || self.profiles.contains(&profile)
    }
}

/// Every agent profile, the expansion of an empty `profiles`. Generated from
/// the `[profiles]` table in `contracts.toml`.
pub const ALL_PROFILES: &[&str] = crate::contracts::PROFILES;

const DRONE: &[&str] = &["drone"];
const GS: &[&str] = &["ground_station"];
/// The flight-controller-bearing profiles that run the swarm and RC lanes.
const FC: &[&str] = &["drone", "ground_station"];
/// The profiles that talk MAVLink to a flight controller or a stage-A hub.
const FC_VISOR: &[&str] = &["drone", "ground_station", "visor"];
/// The camera-owning profiles.
const DRONE_VISOR: &[&str] = &["drone", "visor"];
/// The profiles with a local access point, status panel and buttons.
const GS_VISOR: &[&str] = &["ground_station", "visor"];

/// The all-false row every entry starts from.
const BASE: UnitSpec = UnitSpec {
    name: "",
    category: None,
    profiles: &[],
    roles: &[],
    install: Install::None,
    start_on_install: false,
    health_check: false,
    api_restartable: true,
    drops_link: false,
    headless_keep: false,
    idle_ok: false,
    self_terminating: false,
    teardown: false,
};

/// A boot oneshot the installer enables everywhere. Nothing to restart.
const fn boot_oneshot(name: &'static str) -> UnitSpec {
    UnitSpec {
        name,
        install: Install::Universal,
        api_restartable: false,
        self_terminating: true,
        ..BASE
    }
}

/// A ground-station unit the installer enables, starts and tears down.
const fn gs_unit(name: &'static str, category: Option<Category>) -> UnitSpec {
    UnitSpec {
        name,
        category,
        profiles: GS,
        install: Install::Profile,
        start_on_install: true,
        teardown: true,
        ..BASE
    }
}

/// A sensor-plane unit the supervisor starts on its profiles and an install
/// of any other profile tears down. Each exits 0 when it has nothing to run.
const fn sensor_unit(name: &'static str, profiles: &'static [&'static str]) -> UnitSpec {
    UnitSpec {
        name,
        category: Some(Category::Hardware),
        profiles,
        idle_ok: true,
        teardown: true,
        ..BASE
    }
}

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
    boot_oneshot("ados-fbcon-detach.service"),
    boot_oneshot("ados-display-probe.service"),
    // Confirms a staged CSI overlay or restores the pre-overlay boot config;
    // the probation marker, not the profile, decides whether it does anything.
    boot_oneshot("ados-camera-probe.service"),
    boot_oneshot("ados-usb-otg-host.service"),
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
        ..gs_unit("ados-oled.service", Some(Hardware))
    },
    // The optional I2C status panel self-skips when no panel answers, so the
    // install enables it without kicking it.
    UnitSpec {
        profiles: GS_VISOR,
        start_on_install: false,
        ..gs_unit("ados-oled-i2c.service", Some(Hardware))
    },
    UnitSpec {
        profiles: GS_VISOR,
        health_check: true,
        drops_link: true,
        ..gs_unit("ados-hostapd.service", Some(Hardware))
    },
    UnitSpec {
        profiles: GS_VISOR,
        health_check: true,
        drops_link: true,
        ..gs_unit("ados-dnsmasq-gs.service", Some(Hardware))
    },
    UnitSpec {
        health_check: true,
        idle_ok: true,
        ..gs_unit("ados-setup-captive.service", Some(OnDemand))
    },
    // Exits 0 when the configured display is an LCD or none.
    UnitSpec {
        idle_ok: true,
        ..gs_unit("ados-kiosk.service", Some(Hardware))
    },
    UnitSpec {
        profiles: GS_VISOR,
        drops_link: true,
        ..gs_unit("ados-input.service", Some(Hardware))
    },
    // The front-panel button reader lives in-process here, so a visor runs it
    // too.
    UnitSpec {
        profiles: GS_VISOR,
        drops_link: true,
        ..gs_unit("ados-pic.service", Some(Hardware))
    },
    // The ados-net uplink daemon: uplinks, the Wi-Fi station (it serves the
    // Wi-Fi command socket) and the access-point config the hostapd unit
    // gates on. A visor's AP needs it, so it runs there too.
    UnitSpec {
        profiles: GS_VISOR,
        drops_link: true,
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
        ..BASE
    },
    UnitSpec {
        name: "ados-wfb-relay.service",
        category: Some(Hardware),
        profiles: GS,
        roles: &["relay"],
        drops_link: true,
        idle_ok: true,
        ..BASE
    },
    UnitSpec {
        name: "ados-wfb-receiver.service",
        category: Some(Hardware),
        profiles: GS,
        roles: &["receiver"],
        drops_link: true,
        idle_ok: true,
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

/// The row for `name`, given as the unit file name (`ados-wfb.service`) or
/// its stem (`ados-wfb`).
pub fn find(name: &str) -> Option<&'static UnitSpec> {
    let stem = name.strip_suffix(".service").unwrap_or(name);
    UNITS.iter().find(|u| u.stem() == stem)
}

/// The rows the supervisor manages, in catalog order.
pub fn supervised() -> impl Iterator<Item = &'static UnitSpec> {
    UNITS.iter().filter(|u| u.category.is_some())
}

/// Whether the ados-net uplink daemon (`ados-uplink-router`) runs on
/// `profile`, and so owns the Wi-Fi command socket. Accepts the underscore or
/// the hyphen spelling. Every other node serves that socket from the control
/// surface.
pub fn net_daemon_owns_wifi(profile: &str) -> bool {
    let profile = profile.replace('-', "_");
    find("ados-uplink-router").is_some_and(|u| u.on_profile(&profile))
}

/// The units installed on `profile`, in table order.
pub fn units_for_profile(profile: &str) -> impl Iterator<Item = &'static UnitSpec> + '_ {
    UNITS.iter().filter(move |u| u.on_profile(profile))
}

mod roles;
pub use roles::{role_unit_count, role_unit_names};
