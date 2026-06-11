//! Shared application state handed to every route.
//!
//! The state is cheap to clone (every field is an `Arc` or a small owned value),
//! because the axum router clones it per connection. It holds the pairing reader
//! (also consulted by the LAN-edge auth), the agent version string, the
//! vehicle-state IPC client the status/telemetry routes project, the MAVLink
//! command-send client the command route writes frames through, the logging-store
//! query client + the board sidecar path the status route sources health + board
//! from, and the process start instant used as the status route's uptime
//! fallback. The surface is wired but ships disabled: the systemd unit is deployed
//! off by default and only `ados rust enable control` starts the daemon.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use crate::auth::PairingState;
use crate::ipc::{LogdQueryClient, MavlinkIpcClient, StateIpcClient};
use crate::routes::status::process_uptime_seconds;

/// The agent version string, resolved once at startup, so the native surface
/// reports the exact same version the FastAPI surface does (`/api/version`,
/// `/healthz`, `/api/status`). Resolution order:
/// 1. `ADOS_AGENT_VERSION` env, when a caller/unit pins it explicitly;
/// 2. the `version` the installer records in the install-result contract
///    (`/var/lib/ados/install-result.json`), the real on-box agent version;
/// 3. the crate version, the inert fallback on a dev host or before an install.
pub fn agent_version() -> String {
    if let Ok(v) = std::env::var("ADOS_AGENT_VERSION") {
        if !v.is_empty() {
            return v;
        }
    }
    if let Some(v) = version_from_install_result() {
        return v;
    }
    env!("CARGO_PKG_VERSION").to_string()
}

/// The agent version the installer recorded in the install-result contract, or
/// `None` when the file is absent / unreadable / carries no usable version (a
/// dev host, or before the first install). The path honours `ADOS_INSTALL_RESULT`
/// so a test can redirect it. The `"unknown"` placeholder the installer writes
/// when it could not probe the version is treated as no version.
fn version_from_install_result() -> Option<String> {
    let path = std::env::var("ADOS_INSTALL_RESULT")
        .unwrap_or_else(|_| "/var/lib/ados/install-result.json".to_string());
    version_from_install_result_at(std::path::Path::new(&path))
}

/// Read the `version` field out of an install-result contract file, or `None`
/// when the file is absent / unreadable / not JSON / carries no usable version.
/// The `"unknown"` placeholder the installer writes when it could not probe the
/// version is treated as no version. Pure (path in), so it is unit-testable
/// without mutating the process environment.
fn version_from_install_result_at(path: &std::path::Path) -> Option<String> {
    // The contract is a few hundred bytes; bound the read so a corrupt or
    // tampered file can never balloon memory at startup.
    const MAX_BYTES: u64 = 1024 * 1024;
    if std::fs::metadata(path).ok()?.len() > MAX_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value
        .get("version")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty() && *s != "unknown")
        .map(str::to_string)
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
    /// The MAVLink command-send client the command route writes frames through.
    /// Cheap to clone (the held connection is behind an `Arc<Mutex>`); the route
    /// builds a frame and hands it to this client, which length-prefixes it onto
    /// `/run/ados/mavlink.sock` for the router to forward to the FC.
    pub mavlink: MavlinkIpcClient,
    /// The logging-store query client the status route reads system health from
    /// (CPU / memory / disk / temperature). The continuous collector samples those
    /// into the store; an unreachable store degrades health to its zero default.
    pub logd: LogdQueryClient,
    /// The HAL board sidecar (`/run/ados/board.json`) the status route reads the
    /// full board dict from. The detector persists it; when absent (a fresh boot
    /// before the first write, or a host with no detector running), the status
    /// route reports an empty board object — the same shape the FastAPI route
    /// emits when its own HAL detect raises.
    pub board_path: PathBuf,
    /// The on-disk paths the pairing routes read + write.
    pub pairing_paths: PairingPaths,
    /// When this daemon started, the status route's uptime fallback when the
    /// state snapshot carries no `service_uptime`.
    started: Instant,
}

impl AppState {
    /// Build the state from a pairing reader, a vehicle-state client, the MAVLink
    /// command client, the logging-store query client, the board sidecar path, and
    /// the pairing route paths, resolving the agent version from the environment
    /// and stamping the process start instant.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pairing: Arc<PairingState>,
        state: StateIpcClient,
        mavlink: MavlinkIpcClient,
        logd: LogdQueryClient,
        board_path: PathBuf,
        pairing_paths: PairingPaths,
    ) -> Self {
        Self {
            agent_version: agent_version(),
            pairing,
            state,
            mavlink,
            logd,
            board_path,
            pairing_paths,
            started: Instant::now(),
        }
    }

    /// Seconds since this daemon started. The status route's uptime fallback.
    pub fn process_uptime_seconds(&self) -> f64 {
        process_uptime_seconds(self.started)
    }

    /// Whether the FC is connected, read from the live state snapshot's
    /// `fc_connected` runtime extra (the same field the status + pairing-info
    /// routes read). The command route gates on this: an absent snapshot or a
    /// `false` flag means no FC link, which the route answers with a 503 — the
    /// same posture the FastAPI command route takes when `fc.connected` is false.
    pub fn fc_connected(&self) -> bool {
        self.state
            .snapshot()
            .as_ref()
            .and_then(serde_json::Value::as_object)
            .and_then(|m| m.get("fc_connected"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_reads_the_install_result_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("install-result.json");
        std::fs::write(
            &path,
            r#"{"status":"ok","version":"0.63.0","board":"rpi4b"}"#,
        )
        .unwrap();
        assert_eq!(
            version_from_install_result_at(&path),
            Some("0.63.0".to_string())
        );
    }

    #[test]
    fn version_ignores_the_unknown_placeholder_and_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let unknown = dir.path().join("unknown.json");
        std::fs::write(&unknown, r#"{"version":"unknown"}"#).unwrap();
        assert_eq!(version_from_install_result_at(&unknown), None);
        // Absent file and a non-JSON body both degrade to None.
        assert_eq!(
            version_from_install_result_at(&dir.path().join("absent.json")),
            None
        );
        let garbage = dir.path().join("garbage.json");
        std::fs::write(&garbage, "not json").unwrap();
        assert_eq!(version_from_install_result_at(&garbage), None);
    }
}
