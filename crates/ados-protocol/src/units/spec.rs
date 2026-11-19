//! The unit row type and the shared row templates the table is built from.

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
    /// Takes over something a personal computer's owner already owns: the
    /// console, the display, the Wi-Fi station or routing table, the USB port's
    /// mode, or the mesh. Every list a `HostKind::Personal` install acts on
    /// drops these rows, and the installer tears them down when a host that ran
    /// the appliance set becomes personal.
    pub appliance_only: bool,
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

pub(super) const DRONE: &[&str] = &["drone"];
pub(super) const GS: &[&str] = &["ground_station"];
/// The flight-controller-bearing profiles that run the swarm and RC lanes.
pub(super) const FC: &[&str] = &["drone", "ground_station"];
/// The profiles that talk MAVLink to a flight controller or a stage-A hub.
pub(super) const FC_VISOR: &[&str] = &["drone", "ground_station", "visor"];
/// The camera-owning profiles.
pub(super) const DRONE_VISOR: &[&str] = &["drone", "visor"];
/// The profiles with a local access point, status panel and buttons.
pub(super) const GS_VISOR: &[&str] = &["ground_station", "visor"];

/// The all-false row every entry starts from.
pub(super) const BASE: UnitSpec = UnitSpec {
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
    appliance_only: false,
};

/// A boot oneshot the installer enables everywhere. Nothing to restart.
pub(super) const fn boot_oneshot(name: &'static str) -> UnitSpec {
    UnitSpec {
        name,
        install: Install::Universal,
        api_restartable: false,
        self_terminating: true,
        ..BASE
    }
}

/// A ground-station unit the installer enables, starts and tears down.
pub(super) const fn gs_unit(name: &'static str, category: Option<Category>) -> UnitSpec {
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
pub(super) const fn sensor_unit(name: &'static str, profiles: &'static [&'static str]) -> UnitSpec {
    UnitSpec {
        name,
        category: Some(Category::Hardware),
        profiles,
        idle_ok: true,
        teardown: true,
        ..BASE
    }
}
