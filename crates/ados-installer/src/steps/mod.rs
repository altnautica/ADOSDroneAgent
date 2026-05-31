//! The install steps.
//!
//! Each step is a unit struct implementing [`crate::graph::Step`] with the
//! correct id / requires / checkpoint / kind. The `run` bodies are stubs in
//! this scaffold (they return `Ok`); the real OS work lands in later phases.
//! The dependency edges encoded here ARE the contract: the graph engine relies
//! on them to guarantee the install ordering invariant.
//!
//! The dependency chain (→ means "must run after"):
//!
//! ```text
//!   preflight ── purge_residue (independent, optional)
//!       │
//!      deps
//!       ├── venv_agent ── config_identity ─┐
//!       ├── fetch_binaries ────────────────┤
//!       └── dkms (optional)                │
//!                                          │
//!              systemd  ←── fetch_binaries + config_identity
//!                 │
//!               start  ←── systemd + fetch_binaries
//!                 │
//!               health
//! ```

use crate::graph::Step;

pub mod config_identity;
pub mod deps;
pub mod dkms;
pub mod fetch_binaries;
pub mod health;
pub mod preflight;
pub mod purge_residue;
pub mod start;
pub mod systemd;
pub mod venv_agent;

/// Assemble the full fresh-install step chain. The graph engine orders these
/// by their declared `requires`, so the insertion order here is only the
/// stable tiebreak, not the execution order.
pub fn full_install_chain() -> Vec<Box<dyn Step>> {
    vec![
        Box::new(preflight::Preflight),
        Box::new(purge_residue::PurgeResidue),
        Box::new(deps::Deps),
        Box::new(venv_agent::VenvAgent),
        Box::new(fetch_binaries::FetchBinaries),
        Box::new(dkms::Dkms),
        Box::new(config_identity::ConfigIdentity),
        Box::new(systemd::Systemd),
        Box::new(start::Start),
        Box::new(health::Health),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::topo_order;

    #[test]
    fn full_chain_orders_cleanly() {
        let steps = full_install_chain();
        let order = topo_order(&steps).expect("the install chain must be a valid DAG");
        assert_eq!(order.len(), 10);

        let pos = |id: &str| order.iter().position(|x| x == id).unwrap();
        // Spot-check the load-bearing edges.
        assert!(pos("preflight") < pos("deps"));
        assert!(pos("deps") < pos("venv_agent"));
        assert!(pos("deps") < pos("fetch_binaries"));
        assert!(pos("deps") < pos("dkms"));
        assert!(pos("venv_agent") < pos("config_identity"));
        assert!(pos("fetch_binaries") < pos("systemd"));
        assert!(pos("config_identity") < pos("systemd"));
        assert!(pos("systemd") < pos("start"));
        assert!(pos("fetch_binaries") < pos("start"));
        assert!(pos("start") < pos("health"));
    }
}
