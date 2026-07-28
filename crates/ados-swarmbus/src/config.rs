//! The swarm bus service's runtime configuration.
//!
//! Three fields matter and all three already exist: `agent.profile` decides whether
//! this node transmits or only listens, and `video.wfb.{fleet_id, fleet_slot}` are
//! the fleet addressing the radio plane already uses. Nothing new is invented here
//! — the swarm bus rides the fleet identity that `link_id` is already built from,
//! so a node cannot be addressed differently on the two planes.
//!
//! The defaults come from [`ados_radio::config::WfbConfig::default`] rather than
//! being restated, so the two planes cannot drift apart. `WfbConfig::load_from` is
//! deliberately NOT called: it publishes the *radio* service's config-status
//! sidecar, and a second writer would clobber it.

use std::path::Path;

use ados_radio::config::{fleet_identity_error, FleetIdentityError, WfbConfig};
use serde::Deserialize;

/// Canonical agent config file.
pub const CONFIG_YAML: &str = "/etc/ados/config.yaml";

/// The runtime directory the IPC sockets live under.
fn default_socket_dir() -> String {
    std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string())
}

/// The resolved configuration the daemon runs with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwarmBusConfig {
    /// Raw `agent.profile` (`"drone"` / `"ground_station"` / `"auto"` / absent).
    pub profile: Option<String>,
    /// `agent.device_id`, joined against the slot so the operator surface can name
    /// a neighbour rather than showing a bare number. Empty when absent.
    pub device_id: String,
    /// The operator's monitor-interface pin (`video.wfb.interface`). Usually empty,
    /// in which case the live selection is read from the radio sidecar; see
    /// [`crate::service`].
    pub interface: String,
    /// The fleet every node on this bus shares.
    pub fleet_id: u16,
    /// This node's slot: 0 on a ground station, 1..=24 on a drone.
    pub fleet_slot: u8,
    /// Where the IPC sockets live.
    pub socket_dir: String,
}

impl Default for SwarmBusConfig {
    fn default() -> Self {
        let wfb = WfbConfig::default();
        Self {
            profile: None,
            device_id: String::new(),
            interface: wfb.interface,
            fleet_id: wfb.fleet_id,
            fleet_slot: wfb.fleet_slot,
            socket_dir: default_socket_dir(),
        }
    }
}

/// The `video.wfb` fields this service reads. Every one is `Option` so an absent
/// key falls back to [`WfbConfig::default`] rather than to a second hand-written
/// default that could drift from the radio plane's.
#[derive(Debug, Default, Deserialize)]
struct RawWfb {
    #[serde(default)]
    interface: Option<String>,
    #[serde(default)]
    fleet_id: Option<u16>,
    #[serde(default)]
    fleet_slot: Option<u8>,
}

#[derive(Debug, Default, Deserialize)]
struct RawVideo {
    #[serde(default)]
    wfb: RawWfb,
}

#[derive(Debug, Default, Deserialize)]
struct RawAgent {
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    device_id: String,
}

#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    #[serde(default)]
    agent: RawAgent,
    #[serde(default)]
    video: RawVideo,
}

impl SwarmBusConfig {
    /// Load from the agent config file. A missing file yields defaults; a parse
    /// error is logged loudly and then yields defaults, so the reason a fleet id
    /// looks wrong is in the journal rather than invisible.
    pub fn load_from(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        Self::from_yaml(&text)
    }

    /// Parse from YAML text. Split out from [`Self::load_from`] so the resolution
    /// rules are testable without touching the filesystem.
    pub fn from_yaml(text: &str) -> Self {
        let raw: RawConfig = match serde_norway::from_str(text) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "swarmbus config parse failed; falling back to defaults"
                );
                RawConfig::default()
            }
        };
        let d = Self::default();
        Self {
            profile: raw.agent.profile,
            device_id: raw.agent.device_id,
            interface: raw.video.wfb.interface.unwrap_or(d.interface),
            fleet_id: raw.video.wfb.fleet_id.unwrap_or(d.fleet_id),
            fleet_slot: raw.video.wfb.fleet_slot.unwrap_or(d.fleet_slot),
            socket_dir: d.socket_dir,
        }
    }

    /// Whether this node is a ground station: a listener that never emits a beacon
    /// because it is not an aircraft.
    ///
    /// `"auto"` and an absent profile both read as NOT a ground station, matching
    /// how the rest of the agent treats the raw value — the ground-station profile
    /// is always written explicitly.
    pub fn is_ground_station(&self) -> bool {
        self.profile.as_deref() == Some("ground_station")
    }

    /// Validate the fleet identity, reusing the radio plane's validator so both
    /// planes reject the same configurations for the same reasons.
    ///
    /// A drone with a bad identity must not radiate at all: a duplicate slot
    /// thrashes the wfb-ng FEC decoder about once a second, which presents as
    /// unexplained link loss rather than as an obvious configuration error.
    pub fn identity_error(&self) -> Option<FleetIdentityError> {
        fleet_identity_error(self.fleet_id, self.fleet_slot, self.is_ground_station())
    }

    /// The swarm neighbour-table broadcast socket this service binds.
    pub fn swarm_socket_path(&self) -> String {
        format!("{}/swarm.sock", self.socket_dir.trim_end_matches('/'))
    }

    /// The flight-controller state socket the own-beacon body is filled from.
    pub fn state_socket_path(&self) -> String {
        format!("{}/state.sock", self.socket_dir.trim_end_matches('/'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_radio::config::{FLEET_MAX_SLOTS, SLOT_GROUND};

    #[test]
    fn the_defaults_are_the_radio_planes_defaults_not_a_second_copy() {
        let wfb = WfbConfig::default();
        let c = SwarmBusConfig::default();
        assert_eq!(c.fleet_id, wfb.fleet_id);
        assert_eq!(c.fleet_slot, wfb.fleet_slot);
        assert_eq!(c.interface, wfb.interface);
        // And those are the documented values.
        assert_eq!(c.fleet_id, 1);
        assert_eq!(c.fleet_slot, SLOT_GROUND);
    }

    #[test]
    fn the_fleet_block_is_read_from_the_video_wfb_keys() {
        let c = SwarmBusConfig::from_yaml(
            "agent:\n  profile: drone\n  device_id: ados-abc123\nvideo:\n  wfb:\n    interface: wlan1\n    fleet_id: 7\n    fleet_slot: 3\n",
        );
        assert_eq!(c.profile.as_deref(), Some("drone"));
        assert_eq!(c.device_id, "ados-abc123");
        assert_eq!(c.interface, "wlan1");
        assert_eq!(c.fleet_id, 7);
        assert_eq!(c.fleet_slot, 3);
        assert!(!c.is_ground_station());
        assert_eq!(c.identity_error(), None);
    }

    /// A partially-specified block must take the radio plane's default for the
    /// keys it omits, not zero — `fleet_id: 0` is the reserved unprovisioned value
    /// and would fail validation on a config that never mentioned it.
    #[test]
    fn an_absent_key_falls_back_to_the_default_rather_than_zero() {
        let c = SwarmBusConfig::from_yaml("video:\n  wfb:\n    fleet_slot: 5\n");
        assert_eq!(c.fleet_id, 1, "an unmentioned fleet id is not 0");
        assert_eq!(c.fleet_slot, 5);
        // Entirely empty and entirely absent both give the defaults.
        assert_eq!(SwarmBusConfig::from_yaml(""), SwarmBusConfig::default());
        assert_eq!(SwarmBusConfig::from_yaml("agent: {}\n").fleet_id, 1);
    }

    #[test]
    fn a_ground_station_profile_is_recognised_and_other_values_are_not() {
        let gs = SwarmBusConfig::from_yaml("agent:\n  profile: ground_station\n");
        assert!(gs.is_ground_station());
        assert_eq!(gs.fleet_slot, SLOT_GROUND);
        assert_eq!(gs.identity_error(), None);
        for other in ["drone", "auto", "workstation", "compute"] {
            let c = SwarmBusConfig::from_yaml(&format!("agent:\n  profile: {other}\n"));
            assert!(!c.is_ground_station(), "{other} is not a ground station");
        }
        assert!(!SwarmBusConfig::from_yaml("").is_ground_station());
    }

    /// The identity gate is what stops a misprovisioned drone radiating. Each case
    /// is a real misconfiguration the radio plane also rejects.
    #[test]
    fn a_misprovisioned_identity_is_rejected_with_the_radio_planes_reasons() {
        // A drone left on the ground station's slot 0 — the default a fresh box
        // boots with, and the case that must fail loudest.
        let c = SwarmBusConfig::from_yaml("agent:\n  profile: drone\n");
        assert_eq!(
            c.identity_error(),
            Some(FleetIdentityError::DroneWithoutSlot)
        );

        // The reserved unprovisioned fleet.
        let c = SwarmBusConfig::from_yaml(
            "agent:\n  profile: drone\nvideo:\n  wfb:\n    fleet_id: 0\n    fleet_slot: 2\n",
        );
        assert_eq!(
            c.identity_error(),
            Some(FleetIdentityError::UnprovisionedFleet)
        );

        // A slot past the fleet maximum.
        let c = SwarmBusConfig::from_yaml(&format!(
            "agent:\n  profile: drone\nvideo:\n  wfb:\n    fleet_slot: {}\n",
            FLEET_MAX_SLOTS + 1
        ));
        assert_eq!(
            c.identity_error(),
            Some(FleetIdentityError::SlotOutOfRange(FLEET_MAX_SLOTS + 1))
        );

        // A ground station carrying a drone slot.
        let c = SwarmBusConfig::from_yaml(
            "agent:\n  profile: ground_station\nvideo:\n  wfb:\n    fleet_slot: 4\n",
        );
        assert_eq!(
            c.identity_error(),
            Some(FleetIdentityError::GroundWithDroneSlot(4))
        );

        // And the boundary is inclusive on the legal side.
        let c = SwarmBusConfig::from_yaml(&format!(
            "agent:\n  profile: drone\nvideo:\n  wfb:\n    fleet_slot: {FLEET_MAX_SLOTS}\n"
        ));
        assert_eq!(c.identity_error(), None);
    }

    /// A malformed config must not take the fleet id with it: the parse error is
    /// logged and the defaults stand, so the bus still runs on fleet 1.
    #[test]
    fn a_malformed_config_degrades_to_defaults_rather_than_to_zero() {
        let c = SwarmBusConfig::from_yaml("video:\n  wfb:\n    fleet_id: not-a-number\n");
        assert_eq!(c, SwarmBusConfig::default());
        assert_eq!(c.fleet_id, 1);
    }

    #[test]
    fn socket_paths_resolve_under_the_run_directory_without_doubling_the_slash() {
        let rooted = |dir: &str| SwarmBusConfig {
            socket_dir: dir.to_string(),
            ..SwarmBusConfig::default()
        };
        // A trailing slash must not double up in the join.
        let c = rooted("/run/ados/");
        assert_eq!(c.swarm_socket_path(), "/run/ados/swarm.sock");
        assert_eq!(c.state_socket_path(), "/run/ados/state.sock");
        let c = rooted("/tmp/rig");
        assert_eq!(c.swarm_socket_path(), "/tmp/rig/swarm.sock");
        assert_eq!(c.state_socket_path(), "/tmp/rig/state.sock");
    }
}
