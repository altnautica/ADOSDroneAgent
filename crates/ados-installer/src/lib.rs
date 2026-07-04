//! `ados-installer`: the Rust installer for the ADOS Drone Agent.
//!
//! Replaces the bash `scripts/install.d/*.sh` orchestration with a typed
//! step-graph engine. The crate is structured so the pure-logic core (the
//! step graph, the install-result contract, the argument parser, the
//! checkpoint store, the prebuilt-binary catalog) builds and `cargo test`s on
//! any host; the OS-touching edges are confined to Linux-only `run` bodies
//! that land in later phases.
//!
//! The load-bearing guarantee lives in [`graph`]: when a Required step fails,
//! no later step runs and the install writes a result naming the failure,
//! rather than charging ahead into a half-installed state.

pub mod binaries;
pub mod checkpoint;
pub mod cli;
pub mod ctx;
pub mod env;
pub mod exec;
pub mod graph;
pub mod journal;
pub mod net;
pub mod result;
pub mod steps;
pub mod ui;
pub mod uninstall;
pub mod verify;
pub mod wizard;
