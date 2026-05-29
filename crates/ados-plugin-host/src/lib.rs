//! Plugin RPC host: the server, dispatch gate, and handler routing for the
//! ADOS Drone Agent's plugin sandbox.
//!
//! The host binds one Unix domain socket per plugin, accepts the plugin
//! runner's connection, verifies a per-process capability token at the `hello`
//! handshake, then gates every request on its required capability before
//! routing. The wire — length-prefixed msgpack envelopes and the pipe-delimited
//! HMAC capability token — lives in the `ados-protocol` crate, which is
//! byte-parity tested against the Python supervisor. This crate composes that
//! wire; it re-implements none of it.
//!
//! Scope: the server, the dispatch table, the capability gate, the in-process
//! event bus, and the host-service facade. The full event/ping surface is
//! wired; the 17 host-coupled methods route to a [`host::HostServices`] trait
//! whose default [`host::NoopHost`] returns the `not_implemented` shape,
//! mirroring the Python `_handle_*` stub bodies until the agent's service
//! surfaces expose stable hooks. Plugin lifecycle (install / enable / remove /
//! archive / signing / state) and the plugin SDK are deliberate follow-ons and
//! are not part of this crate.
//!
//! Modules:
//! - [`dispatch`] — the `method -> (handler, required_cap)` table and the gate
//!   producing the exact wire error strings.
//! - [`handlers`] — the in-process event bus, topic matching, the per-topic
//!   publish/subscribe checks, and host-method routing.
//! - [`host`] — the [`host::HostServices`] facade trait and [`host::NoopHost`].
//! - [`server`] — the per-plugin socket server: handshake, dispatch loop, and
//!   event push path.

pub mod dispatch;
pub mod handlers;
pub mod host;
pub mod server;

pub use dispatch::{gate, Gate, Method};
pub use handlers::{Event, EventBus};
pub use host::{HostResult, HostServices, NoopHost};
pub use server::{PluginIpcServer, ServerError, DEFAULT_SOCKET_DIR};
