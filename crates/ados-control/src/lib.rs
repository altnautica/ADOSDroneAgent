//! Native HTTP control surface for the agent.
//!
//! The same status/pairing/command API the GCS speaks, served on the trusted
//! local Unix socket and the LAN TCP port with no Python runtime. It answers the
//! agent's `/api/*` (+ `/healthz`) routes byte-identically to the FastAPI
//! surface, so the same GCS works against either. On the full agent it binds an
//! alternate socket + port and is inert (the GCS still uses FastAPI on `:8080`);
//! on the headless profile it binds `:8080` itself.
//!
//! This crate carries the dual-listener serve loop ([`serve`]), the LAN-edge
//! auth ([`auth`]), the shared app state ([`state`]), the route Router
//! ([`routes`]), and the daemon lifecycle ([`run_with_paths`]). The binary is
//! functional but ships dark — no supervisor registration and no systemd unit
//! enable it yet — until the install layer wires it.

pub mod armed_guard;
pub mod auth;
pub mod battery;
pub mod config;
pub mod config_store;
pub mod daemon;
pub mod dashboard_pin;
pub mod fc_target;
pub mod hw_local;
pub mod ipc;
pub mod mcp;
pub mod mdns;
pub(crate) mod mediamtx_probe;
pub mod pairing_store;
pub mod param_store;
pub mod probe;
pub mod profile;
pub mod proxy;
pub mod proxy_auth;
pub mod routes;
pub mod routing;
pub mod runtime_mode;
pub mod security_state;
pub mod serve;
pub(crate) mod sidecar;
pub mod state;
pub mod vehicle_guard;
mod wfb_pair_state;

pub use daemon::{run_daemon, run_with_paths, DaemonPaths, WATCHDOG_PING_INTERVAL};

/// Canonical runtime paths. The control socket lives under `/run/ados` (tmpfs);
/// the pairing-state file lives under `/etc/ados` (persistent). The TCP port is
/// the LAN front the GCS reaches the agent on.
pub mod paths {
    /// Control socket: the trusted local plane (the on-box CLI).
    pub const CONTROL_SOCKET: &str = "/run/ados/control.sock";
    /// The LAN TCP port the native front owns. `ADOS_CONTROL_PORT` overrides it.
    pub const FRONT_TCP_PORT: u16 = 8080;
}
