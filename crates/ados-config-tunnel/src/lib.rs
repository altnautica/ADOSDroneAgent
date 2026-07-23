//! `ados-config-tunnel` — the config-over-radio substrate (config request/
//! response over a MAVLink TUNNEL on the low-rate `-p1` control-plane bearer).
//!
//! Two agent-side endpoints on one binary, selected by profile:
//!
//! - **drone → terminator** ([`terminator`]): receives config-typed TUNNEL
//!   frames off the bearer, reassembles a request, calls the local
//!   `/api/config` surface on `:8080`, and chunks the reply back onto the
//!   downlink. It restricts every call to `/api/config` exactly, so the
//!   channel can only read/write agent config — never a general command proxy
//!   or an armed-flight authority.
//! - **ground station → injector** ([`injector`] + [`cmdsock`]): accepts a
//!   config request over its command socket (which `ados-control`'s
//!   relayed-config route forwards to), chunks it onto the bearer, and awaits
//!   the drone's reassembled reply via a `request_id` pending-map.
//!
//! ## Ships inert (default off)
//!
//! Both halves are gated off by default ([`config::TunnelChannelConfig`]):
//! `radio.tunnel.enabled` opts the channel in, `radio.tunnel.command_enabled`
//! separately opens config WRITES (reads-only until then), and the systemd
//! unit gates on the `/etc/ados/tunnel-enabled` marker that mirrors the config.
//! Nothing radiates or accepts config-over-radio until it is explicitly
//! enabled after a safety review.
//!
//! ## Bearer boundary
//!
//! The wire framing ([`ados_protocol::tunnel_config`] + the TUNNEL codec) and
//! the local UDP transport ([`transport`]) are built here; bridging the
//! service's dedicated local UDP ports onto the `-p1` WFB control plane is a
//! separate, gated `ados-radio` integration — this crate never touches the raw
//! WFB sockets or the FC lane.

use std::time::Duration;

pub mod cmdsock;
pub mod config;
pub mod config_client;
pub mod injector;
pub mod message;
pub mod paths;
pub mod sidecar;
pub mod stats;
pub mod terminator;
pub mod transport;

/// Largest number of TUNNEL frames one config message may span. Bounds both
/// the chunker and the reassembler; above [`message::MAX_CONFIG_RESPONSE_BYTES`]
/// worth of chunks with headroom, well below anything that could flood the
/// low-rate lane.
pub const MAX_CHUNKS: usize = 64;

/// Hard cap on a reassembled body (above the response cap, so a legitimate
/// message never trips it while a flood is still bounded).
pub const MAX_BODY_BYTES: usize = 8 * 1024;

/// How long an in-flight message may wait for its remaining chunks before the
/// reassembler drops it. A config op is small and idempotent; a message that
/// has not completed in this window is a lost transfer, not a slow one.
pub const REASSEMBLY_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the reassembler sweeps out timed-out in-flight messages.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(2);
