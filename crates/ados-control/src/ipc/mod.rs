//! IPC client seams the route surface reads.
//!
//! The status and telemetry routes do not hold vehicle state themselves; the
//! MAVLink service owns it and publishes a snapshot on `/run/ados/state.sock`.
//! [`state_client`] is the read side of that seam: it connects, decodes the
//! self-describing snapshot frame (newline JSON or length-prefixed msgpack), and
//! holds the latest snapshot for a route to project. It only ever reads, and a
//! missing socket is normal (an idle or unpaired agent before the state hub is
//! up), so the routes degrade to an empty snapshot rather than fail.

pub mod state_client;

pub use state_client::StateIpcClient;
