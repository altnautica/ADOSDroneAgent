//! Atlas world-model stream lane (tier 3 transport).
//!
//! The keyframe stream (drone -> compute) and the world-model descriptor stream
//! (compute -> GCS) get their own lane so the world model never competes with
//! the bounded MAVLink queue or the video pipeline. The same framed [`AtlasEvent`] rides any
//! bearer; the bearer is chosen by topology through a priority failover ladder
//! the same way the network uplink matrix picks an uplink:
//!
//! 1. **Direct LAN/WiFi** ([`LanHttpBearer`]) — first-class, built first. The
//!    drone, compute node, and GCS share a network; keyframes stream direct over
//!    LAN HTTP. A real indoor-commercial production topology and the lead-testable
//!    path (local-first).
//! 2. **Post-flight LAN bulk** — the landed drone bulk-uploads the full bag.
//! 3. **WFB relay** — the ground agent bridges a decimated lane WFB<->LAN
//!    (the carrier lands with the ground-agent relay role).
//! 4. **Cloud relay** — MQTT/Convex for off-LAN reach, an opt-in cloud lane.
//!
//! [`LoopbackBearer`] is the in-process bearer for tests and the same-host case.
//! [`WorldBroadcaster`] is the compute-side fan-out a world-model consumer (the
//! GCS Live World, or the drone republishing onto its own plugin bus) subscribes
//! to over a per-device WebSocket. All carry the identical envelope, so swapping
//! a bearer never changes the world-model contract.
//!
//! The compute->GCS lane carries generation-versioned world-model DESCRIPTORS,
//! not splat deltas. See [`world_stream`] for why: SPZ and SOG are whole-scene
//! containers with global quantisation and Morton ordering, and neither the
//! formats nor the Khronos glTF extensions define an incremental append or delta
//! codec, so the specified delta lane would have required inventing a codec
//! nothing else reads.

mod bearer;
mod error;
mod ladder;
mod lan_http;
mod loopback;
mod wfb_relay;
mod world_stream;

pub use bearer::{AtlasBearer, BearerKind};
pub use error::TransportError;
pub use ladder::BearerLadder;
pub use lan_http::{atlas_event_router, LanHttpBearer};
pub use loopback::LoopbackBearer;
pub use wfb_relay::{WfbRelayBearer, WFB_MAX_DATAGRAM};
pub use world_stream::{world_ws_path, world_ws_router, WorldBroadcaster, WORLD_WS_ROUTE};

// Re-export the framed event the lane carries so callers get one import surface.
pub use ados_protocol::atlas::AtlasEvent;
