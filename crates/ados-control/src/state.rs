//! Shared application state handed to every route.
//!
//! The state is cheap to clone (every field is an `Arc` or a small owned value),
//! because the axum router clones it per connection. It holds the pairing reader
//! (also consulted by the LAN-edge auth), the agent version string, the
//! vehicle-state IPC client the status/telemetry routes project, and the process
//! start instant used as the status route's uptime fallback. The command/pairing
//! IPC handles that land in later chunks add their fields here; they are
//! deliberately absent rather than stubbed.

use std::path::PathBuf;
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

/// The HAL-detected board name the pairing-info route reports. Read from
/// `ADOS_BOARD_NAME`, which the systemd unit injects from the Python HAL
/// `detect_board()` (the native surface has no in-process HAL-detect port). The
/// `"unknown"` fallback is correct when the env is unset (a dev host, or this
/// crate's pre-wiring/inert state where no unit injects the env yet), matching
/// the FastAPI route's `app.board_name or "unknown"` fallback shape. Follows the
/// same env-resolution pattern as `agent_version()`.
pub fn board_name() -> String {
    std::env::var("ADOS_BOARD_NAME").unwrap_or_else(|_| "unknown".to_string())
}

/// The native-vs-packaged runtime badge the pairing-info route reports. Read from
/// `ADOS_RUNTIME_MODE`, which the systemd unit injects from the Python
/// `compute_runtime_mode(profile)`. The `"packaged"` fallback is correct when the
/// env is unset (a pre-cutover agent, and this surface is itself inert until the
/// cutover), matching the FastAPI route's default. Follows the same
/// env-resolution pattern as `agent_version()`.
pub fn runtime_mode() -> String {
    std::env::var("ADOS_RUNTIME_MODE").unwrap_or_else(|_| "packaged".to_string())
}

/// The on-disk paths the pairing routes read and write. Cloned per connection
/// (it is small + cheap). The pairing-info route reads the config, the pairing
/// document, the radio key dir, and the bind-state sidecar live on each request
/// (mirroring the FastAPI route, which reads the live runtime), so the paths —
/// not pre-read snapshots — live in the state. Each is injectable so a test
/// redirects them at a tempdir.
#[derive(Clone, Debug)]
pub struct PairingPaths {
    /// The agent config (`/etc/ados/config.yaml`) the pairing-info route projects
    /// for device identity, profile, and the radio peer.
    pub config: PathBuf,
    /// The pairing-state document the info route reads and the claim/unpair
    /// handlers write.
    pub pairing_json: PathBuf,
    /// The WFB key directory (`/etc/ados/wfb`); the presence of `tx.key`/`rx.key`
    /// is the `radio_paired` signal.
    pub wfb_key_dir: PathBuf,
    /// The WFB bind-session sentinel (`/run/ados/bind-state.json`) the info route
    /// folds into `bind_state`.
    pub bind_state: PathBuf,
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
    /// The on-disk paths the pairing routes read + write.
    pub pairing_paths: PairingPaths,
    /// When this daemon started, the status route's uptime fallback when the
    /// state snapshot carries no `service_uptime`.
    started: Instant,
}

impl AppState {
    /// Build the state from a pairing reader, a vehicle-state client, and the
    /// pairing route paths, resolving the agent version from the environment and
    /// stamping the process start instant. The command IPC handle a later chunk
    /// needs is added to this struct then, not faked here.
    pub fn new(
        pairing: Arc<PairingState>,
        state: StateIpcClient,
        pairing_paths: PairingPaths,
    ) -> Self {
        Self {
            agent_version: agent_version(),
            pairing,
            state,
            pairing_paths,
            started: Instant::now(),
        }
    }

    /// Seconds since this daemon started. The status route's uptime fallback.
    pub fn process_uptime_seconds(&self) -> f64 {
        process_uptime_seconds(self.started)
    }
}
