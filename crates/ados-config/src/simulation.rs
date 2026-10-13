//! Whether the node is running against a simulator.
//!
//! `simulation.enabled` is set by whoever wires the node to a simulated flight
//! controller and camera. The status routes report it as `sim` so a ground
//! station can mark the node as simulated rather than leave an operator to
//! guess whether the telemetry on screen comes from a real airframe.
//!
//! Absent or malformed resolves to **false**: a node only claims to be
//! simulated when its config says so explicitly.

use std::path::Path;

use crate::log_store::CONFIG_YAML;

/// The parsed slice of `simulation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SimulationGate {
    pub enabled: bool,
}

/// Parse `simulation.enabled` out of a config body.
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
    }

    let raw: Raw = crate::yaml_or_default(text, "simulation");
    SimulationGate {
        enabled: raw.simulation.map(|s| s.enabled).unwrap_or(false),
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
}
