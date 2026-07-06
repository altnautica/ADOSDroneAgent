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

/// The agent version string, resolved live on each call (through
/// [`AppState::agent_version`]), so the native surface reports the exact same
/// version the FastAPI surface does (`/api/version`, `/healthz`, `/api/status`)
/// and tracks an upgrade without a front restart. Resolution order:
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

/// The HAL-detected board name the pairing-info route reports. Read live from the
/// `name` field of the board sidecar (`/run/ados/board.json`) the detector
/// persists — the same on-disk source the status route's board block reads. The
/// FastAPI route reports `app.board_name or "unknown"`, where `app.board_name` is
/// the HAL `board.name` (e.g. `"Raspberry Pi 4B"`); reading the sidecar's `name`
/// field matches that exactly. Falls back to `"unknown"` only when the sidecar is
/// absent / unreadable / carries no usable `name` (a fresh boot before the first
/// status write, or a host with no detector running).
pub fn board_name(board_path: &std::path::Path) -> String {
    board_name_at(board_path).unwrap_or_else(|| "unknown".to_string())
}

/// Read the `name` field out of a board sidecar file, or `None` when the file is
/// absent / unreadable / not a JSON object / carries no non-empty `name`. Pure
/// (path in), so it is unit-testable without mutating the process environment.
fn board_name_at(path: &std::path::Path) -> Option<String> {
    // The sidecar is a small board dict; bound the read so a corrupt or tampered
    // file can never balloon memory on the pairing-info hot path.
    const MAX_BYTES: u64 = 1024 * 1024;
    if std::fs::metadata(path).ok()?.len() > MAX_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value
        .get("name")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The native-vs-packaged runtime badge the pairing-info route reports. The
/// native surface has no in-process port of the Python `compute_runtime_mode`, so
/// the Python API writes the computed value to the `runtime-mode` sidecar at
/// startup and this reads it live. Resolution order:
/// 1. the `runtime-mode` sidecar under `ADOS_RUN_DIR` (`/run/ados/runtime-mode`),
///    the value the Python API computed for this node's profile;
/// 2. the `ADOS_RUNTIME_MODE` env, when a caller/unit pins it explicitly;
/// 3. the `"packaged"` fallback, correct for a pre-cutover agent (and this
///    surface is itself inert until the cutover), matching the FastAPI default.
pub fn runtime_mode() -> String {
    let run_dir = std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string());
    if let Some(mode) = runtime_mode_at(&std::path::Path::new(&run_dir).join("runtime-mode")) {
        return mode;
    }
    if let Ok(v) = std::env::var("ADOS_RUNTIME_MODE") {
        if !v.is_empty() {
            return v;
        }
    }
    "packaged".to_string()
}

/// Read the runtime-mode string out of the sidecar file, or `None` when the file
/// is absent / unreadable / empty. The Python API writes a single line of plain
/// text (no trailing structure), so the body is trimmed and taken whole. Pure
/// (path in), so it is unit-testable without mutating the process environment.
fn runtime_mode_at(path: &std::path::Path) -> Option<String> {
    // The sidecar is one short word; bound the read so a corrupt file can never
    // balloon memory on the pairing-info hot path.
    const MAX_BYTES: u64 = 4096;
    if std::fs::metadata(path).ok()?.len() > MAX_BYTES {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
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
    /// The profile-source sentinel (`/etc/ados/profile.conf`) the profile resolver
    /// falls back to when `agent.profile` is `auto`/empty. Threaded so the resolve
    /// is path-injectable (a test points it at a tempdir).
    pub profile_conf: PathBuf,
    /// The ground-station role sentinel (`/etc/ados/mesh/role`) the profile resolver
    /// reads for a ground station. Threaded so the resolve is path-injectable.
    pub mesh_role: PathBuf,
}

/// State shared across all routes. Cloned per connection by the axum router, so
/// every field is an `Arc` or a small owned value.
#[derive(Clone)]
pub struct AppState {
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
    /// the pairing route paths, stamping the process start instant. The agent
    /// version is resolved live per request (see [`AppState::agent_version`]), not
    /// cached here, so an upgrade is reflected without a front restart.
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
            pairing,
            state,
            mavlink,
            logd,
            board_path,
            pairing_paths,
            started: Instant::now(),
        }
    }

    /// The agent version reported by `/healthz`, `/api/version`, and the status
    /// payloads — resolved LIVE per call (`ADOS_AGENT_VERSION` env, then the
    /// install-result contract, then the crate version), NOT cached at
    /// construction. The installer starts the front before it finalizes
    /// `install-result.json`, so a startup snapshot reports the pre-upgrade version
    /// until the next front restart; reading live picks the new version up on the
    /// next request. Cheap: the contract is a few hundred bytes, read only on the
    /// version/status routes. (Bare call resolves to the free `agent_version`.)
    pub fn agent_version(&self) -> String {
        agent_version()
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

    /// The autopilot family the FC advertises, read from the live state
    /// snapshot's `autopilot` field (the `HEARTBEAT.autopilot` wire value the
    /// MAVLink service records). The command route uses it to pick the mode
    /// encoding: `12` is PX4 (its own packed `(main, sub)` scheme), anything else
    /// is treated as ArduPilot (the copter mode table). An absent snapshot or a
    /// missing field reads `0` (unknown → the ArduPilot default path).
    pub fn autopilot(&self) -> i64 {
        self.state
            .snapshot()
            .as_ref()
            .and_then(serde_json::Value::as_object)
            .and_then(|m| m.get("autopilot"))
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0)
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

    #[test]
    fn board_name_reads_the_sidecar_name_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("board.json");
        // The on-rig sidecar shape: a full board dict whose `name` field is the
        // friendly HAL board name the FastAPI route reports as `app.board_name`.
        std::fs::write(
            &path,
            r#"{"arch":"aarch64","cpu_cores":4,"model":"Raspberry Pi 4 Model B Rev 1.5","name":"Raspberry Pi 4B"}"#,
        )
        .unwrap();
        assert_eq!(board_name_at(&path), Some("Raspberry Pi 4B".to_string()));
        // The public helper resolves the same value.
        assert_eq!(board_name(&path), "Raspberry Pi 4B");
    }

    #[test]
    fn board_name_falls_back_to_unknown_when_absent_or_unusable() {
        let dir = tempfile::tempdir().unwrap();
        // Absent file → None → "unknown".
        let absent = dir.path().join("absent.json");
        assert_eq!(board_name_at(&absent), None);
        assert_eq!(board_name(&absent), "unknown");
        // An empty `name` is treated as no name.
        let empty = dir.path().join("empty-name.json");
        std::fs::write(&empty, r#"{"name":""}"#).unwrap();
        assert_eq!(board_name_at(&empty), None);
        // A dict with no `name`, a non-object body, and non-JSON all degrade.
        let no_name = dir.path().join("no-name.json");
        std::fs::write(&no_name, r#"{"arch":"aarch64"}"#).unwrap();
        assert_eq!(board_name_at(&no_name), None);
        let arr = dir.path().join("array.json");
        std::fs::write(&arr, "[1,2,3]").unwrap();
        assert_eq!(board_name_at(&arr), None);
        let garbage = dir.path().join("garbage.json");
        std::fs::write(&garbage, "not json").unwrap();
        assert_eq!(board_name_at(&garbage), None);
    }

    #[test]
    fn runtime_mode_reads_the_sidecar_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime-mode");
        // The Python API writes one line of plain text; a trailing newline is
        // trimmed so the value matches `compute_runtime_mode` exactly.
        std::fs::write(&path, "hybrid\n").unwrap();
        assert_eq!(runtime_mode_at(&path), Some("hybrid".to_string()));
        std::fs::write(&path, "native").unwrap();
        assert_eq!(runtime_mode_at(&path), Some("native".to_string()));
    }

    #[test]
    fn runtime_mode_sidecar_is_none_when_absent_or_empty() {
        let dir = tempfile::tempdir().unwrap();
        // Absent file degrades to None (the caller then tries the env / default).
        assert_eq!(runtime_mode_at(&dir.path().join("absent")), None);
        // An empty / whitespace-only body is treated as no value.
        let empty = dir.path().join("empty");
        std::fs::write(&empty, "   \n").unwrap();
        assert_eq!(runtime_mode_at(&empty), None);
    }
}
