//! Shared application state handed to every route.
//!
//! The state is cheap to clone (every field is an `Arc` or a small owned value),
//! because the axum router clones it per connection. It holds the pairing reader
//! (also consulted by the LAN-edge auth), the agent version string, the
//! vehicle-state IPC client the status/telemetry routes project, and the process
//! start instant used as the status route's uptime fallback. The command/pairing
//! IPC handles that land in later chunks add their fields here; they are
//! deliberately absent rather than stubbed.

use std::sync::Arc;
use std::time::Instant;

use crate::auth::PairingState;
use crate::ipc::StateIpcClient;
use crate::routes::status::process_uptime_seconds;

/// The agent version string, resolved once at startup. The systemd unit sets
/// `ADOS_AGENT_VERSION` from the Python `ados.__version__` source so the native
/// surface reports the exact same version the FastAPI surface does; the crate
/// version is the inert fallback when the env is unset (a dev host, or this
/// crate's pre-wiring state where no unit injects the env yet). Mirrors the
/// `agent_version()` helper the plugin-host binary uses.
pub fn agent_version() -> String {
    std::env::var("ADOS_AGENT_VERSION").unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string())
}

/// State shared across all routes. Cloned per connection by the axum router, so
/// every field is an `Arc` or a small owned value.
#[derive(Clone)]
pub struct AppState {
    /// The agent version, reported by `/healthz` and `/api/version`.
    pub agent_version: String,
    /// The pairing-state reader, shared with the LAN-edge auth middleware so a
    /// route and the gate read the same short-TTL-cached posture.
    pub pairing: Arc<PairingState>,
    /// The vehicle-state IPC client the status + telemetry routes project. Cheap
    /// to clone (the snapshot is held behind an `Arc`).
    pub state: StateIpcClient,
    /// When this daemon started, the status route's uptime fallback when the
    /// state snapshot carries no `service_uptime`.
    started: Instant,
}

impl AppState {
    /// Build the state from a pairing reader and a vehicle-state client,
    /// resolving the agent version from the environment and stamping the process
    /// start instant. The command/pairing IPC handles the later chunks need are
    /// added to this struct then, not faked here.
    pub fn new(pairing: Arc<PairingState>, state: StateIpcClient) -> Self {
        Self {
            agent_version: agent_version(),
            pairing,
            state,
            started: Instant::now(),
        }
    }

    /// Seconds since this daemon started. The status route's uptime fallback.
    pub fn process_uptime_seconds(&self) -> f64 {
        process_uptime_seconds(self.started)
    }
}
