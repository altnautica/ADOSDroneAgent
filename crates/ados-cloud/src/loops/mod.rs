//! The periodic cloud-relay loops: heartbeat status push, command poll, and the
//! pairing beacon. Each loop gates on the paired state and the configured
//! convex URL, and authenticates with the `X-ADOS-Key` header. Ports the loop
//! bodies in `src/ados/services/cloud/`.
//!
//! [`atlas_forwarder`] is the odd one out — it does not POST to Convex. It
//! subscribes to the local atlas bus and forwards world-model events to a
//! compute node over the bearer ladder (LAN -> WFB relay -> cloud), local-first,
//! and is INERT unless Atlas is enabled.

pub mod atlas_forwarder;
pub mod beacon;
pub mod command_poll;
pub mod enrichment;
pub mod heartbeat;

pub use beacon::{beacon_enabled, build_beacon_body, BeaconInputs, DEFAULT_BEACON_INTERVAL};
pub use command_poll::{build_ack, parse_commands, POLL_INTERVAL};
pub use enrichment::{build_native_enrichment, CpuSample};
pub use heartbeat::{build_payload, post_heartbeat, HeartbeatBase, HEARTBEAT_INTERVAL};
