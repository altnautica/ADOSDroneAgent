//! Whether the node is running against a simulator, and which clock it runs on.
//!
//! `simulation.enabled` is set by whoever wires the node to a simulated flight
//! controller and camera. The status routes report it as `sim` so a ground
//! station can mark the node as simulated rather than leave an operator to
//! guess whether the telemetry on screen comes from a real airframe.
//!
//! `simulation.clock: sim` puts the agent's control loops on the World Engine
//! sim-clock feed (`simclock.v1`, received on `simulation.clock_feed`). It only
//! takes effect while `enabled` is true; a real node always runs on wall time.
//!
//! Absent or malformed resolves to **false** and **wall**: a node only claims
//! to be simulated when its config says so explicitly.

use std::net::SocketAddr;
use std::path::Path;

use crate::log_store::CONFIG_YAML;

/// Which clock the agent's control loops read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClockKind {
    #[default]
    Wall,
    Sim,
}

/// How the agent follows the sim clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClockMode {
    /// Run on the broadcast time.
    #[default]
    Follow,
    /// Also acknowledge each grant so the simulator waits for the agent.
    Lockstep,
}

/// The parsed slice of `simulation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SimulationGate {
    pub enabled: bool,
    /// As written; read through [`SimulationGate::sim_clock`].
    pub clock: ClockKind,
    pub clock_feed: Option<SocketAddr>,
    pub clock_mode: ClockMode,
}

impl SimulationGate {
    /// The sim clock is on only on a simulated node that asks for it.
    pub fn sim_clock(&self) -> bool {
        self.enabled && self.clock == ClockKind::Sim
    }
}

/// Parse `simulation` out of a config body.
pub fn read_gate_from(text: &str) -> SimulationGate {
    #[derive(serde::Deserialize, Default)]
    struct Raw {
        #[serde(default)]
        simulation: Option<Simulation>,
    }
    #[derive(serde::Deserialize)]
    struct Simulation {
        #[serde(default)]
        enabled: bool,
        #[serde(default)]
        clock: ClockKind,
        #[serde(default)]
        clock_feed: Option<String>,
        #[serde(default)]
        clock_mode: ClockMode,
    }

    let raw: Raw = crate::yaml_or_default(text, "simulation");
    match raw.simulation {
        Some(s) => SimulationGate {
            enabled: s.enabled,
            clock: s.clock,
            clock_feed: s.clock_feed.and_then(|a| a.parse().ok()),
            clock_mode: s.clock_mode,
        },
        None => SimulationGate::default(),
    }
}

/// Read the gate from a config file. A missing file is not simulated.
pub fn read_gate(path: &Path) -> SimulationGate {
    match std::fs::read_to_string(path) {
        Ok(text) => read_gate_from(&text),
        Err(_) => SimulationGate::default(),
    }
}

/// Read the gate from the canonical config path.
pub fn simulation_enabled() -> bool {
    read_gate(Path::new(CONFIG_YAML)).enabled
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_node_is_not_simulated() {
        assert!(!read_gate_from("agent:\n  name: x\n").enabled);
        assert!(!SimulationGate::default().enabled);
    }

    #[test]
    fn the_key_turns_it_on_and_off() {
        assert!(read_gate_from("simulation:\n  enabled: true\n").enabled);
        assert!(!read_gate_from("simulation:\n  enabled: false\n").enabled);
    }

    #[test]
    fn a_config_that_does_not_parse_is_not_simulated() {
        assert!(!read_gate_from(": : : not yaml").enabled);
        assert!(!read_gate_from("simulation: [a, b]").enabled);
    }

    #[test]
    fn clock_defaults_to_wall() {
        let g = read_gate_from("simulation:\n  enabled: true\n");
        assert_eq!(g.clock, ClockKind::Wall);
        assert_eq!(g.clock_mode, ClockMode::Follow);
        assert_eq!(g.clock_feed, None);
        assert!(!g.sim_clock());
    }

    #[test]
    fn clock_sim_needs_enabled() {
        let on = read_gate_from(
            "simulation:\n  enabled: true\n  clock: sim\n  clock_feed: 127.0.0.1:14650\n  clock_mode: lockstep\n",
        );
        assert!(on.sim_clock());
        assert_eq!(on.clock_feed, Some("127.0.0.1:14650".parse().unwrap()));
        assert_eq!(on.clock_mode, ClockMode::Lockstep);
        let off = read_gate_from("simulation:\n  enabled: false\n  clock: sim\n");
        assert_eq!(off.clock, ClockKind::Sim);
        assert!(!off.sim_clock());
    }
}
