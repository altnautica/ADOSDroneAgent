//! Atlas world-model stream lane (tier 3 transport).
//!
//! The keyframe stream (drone -> compute) and the splat-delta stream (compute ->
//! GCS) get their own lane so the world model never competes with the bounded
//! MAVLink queue or the video pipeline. The same framed [`AtlasEvent`] rides any
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
//! [`DeltaBroadcaster`] is the compute-side fan-out the GCS Live World subscribes
//! to over a WebSocket. All carry the identical envelope, so swapping a bearer
//! never changes the world-model contract.

mod bearer;
mod delta;
mod error;
mod ladder;
mod lan_http;
mod loopback;

pub use bearer::{AtlasBearer, BearerKind};
pub use delta::{delta_ws_path, delta_ws_router, DeltaBroadcaster, DELTA_WS_ROUTE};
pub use error::TransportError;
pub use ladder::BearerLadder;
pub use lan_http::{atlas_event_router, LanHttpBearer};
pub use loopback::LoopbackBearer;

// Re-export the framed event the lane carries so callers get one import surface.
pub use ados_protocol::atlas::AtlasEvent;
