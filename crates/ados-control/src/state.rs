//! Shared application state handed to every route.
//!
//! The state is cheap to clone (every field is an `Arc` or a small owned value),
//! because the axum router clones it per connection. This foundation chunk holds
//! only what the `/healthz` and `/api/version` routes need: the pairing reader
//! (also consulted by the LAN-edge auth) and the agent version string. The
//! status/pairing/command routes that land in later chunks add their IPC client
//! handles here; they are deliberately absent rather than stubbed.

use std::sync::Arc;

use crate::auth::PairingState;

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
}

impl AppState {
    /// Build the state from a pairing reader, resolving the agent version from
    /// the environment. The IPC client handles the later chunks need are added
    /// to this struct then, not faked here.
    pub fn new(pairing: Arc<PairingState>) -> Self {
        Self {
            agent_version: agent_version(),
            pairing,
        }
    }
}
