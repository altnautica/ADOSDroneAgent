//! Flight-controller link and vehicle-state producer for the ADOS Drone Agent.
//!
//! This crate is the Rust home of the `ados-mavlink` service: it owns the FC
//! serial link, fans every MAVLink frame to the MAVLink IPC socket, and
//! produces the vehicle-state snapshot on the state socket (Contracts A and B
//! in `ados-protocol`).
//!
//! The build lands incrementally. The first module is [`state`] — the vehicle
//! state aggregator that turns the MAVLink message stream into the JSON
//! snapshot the REST layer and cloud relay consume. It is intentionally free of
//! I/O so it can be unit-tested against constructed messages for byte-level
//! parity with the Python producer.

pub mod config;
pub mod connection;
pub mod param_cache;
pub mod state;
