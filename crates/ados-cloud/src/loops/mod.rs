//! The periodic cloud-relay loops: heartbeat status push, command poll, and the
//! pairing beacon. Each loop gates on the paired state and the configured
//! convex URL, and authenticates with the `X-ADOS-Key` header. Ports the loop
//! bodies in `src/ados/services/cloud/`.

pub mod beacon;
pub mod command_poll;
pub mod heartbeat;

pub use beacon::{beacon_enabled, build_beacon_body, BeaconInputs, DEFAULT_BEACON_INTERVAL};
pub use command_poll::{build_ack, parse_commands, POLL_INTERVAL};
pub use heartbeat::{
    build_payload, post_heartbeat, read_enrichment, HeartbeatBase, HEARTBEAT_INTERVAL,
};
