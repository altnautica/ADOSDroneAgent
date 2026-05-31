//! Local-radio auto-pair: drive the first-boot WFB bind IN-PROCESS.
//!
//! This lives in the supervisor, not the cloud relay where it used to run. The
//! bind FSM stops + starts the `ados-wfb` unit to flip wfb-ng profiles, so the
//! trigger cannot live in the radio service (it would kill itself). The earlier
//! home was `ados-cloud` only because "the cloud relay does not touch the
//! radio" — but that is a LAN-only function parked in a service that is idle in
//! local-first mode (cloud relay off by default). The supervisor is the unit
//! that does the stopping (never stopped itself), already owns the
//! [`BindOrchestrator`], and always runs, so it is the correct host. The bind
//! runs in-process via the orchestrator directly, with no control-socket
//! round-trip.
//!
//! Each tick, while armed (`video.wfb.auto_pair_enabled`) and not already paired
//! (the role's key file is absent), run one bind. A successful bind writes the
//! key file, so the next tick sees the rig as paired and stops attempting; the
//! key-apply step also flips `auto_pair_enabled` to false in the config.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use crate::bind::orchestrator::{BindOrchestrator, BindStartError};
use crate::bind::BindRole;

/// The operator config the arm flag is read from each tick.
const CONFIG_YAML: &str = "/etc/ados/config.yaml";
/// Settle delay before the first attempt (lets the radio + units come up).
pub const START_DELAY: Duration = Duration::from_secs(15);
/// Backoff between attempts.
pub const RETRY_BACKOFF: Duration = Duration::from_secs(60);

/// Cap on consecutive bind attempts before the rig stops retrying the local
/// radio bind and asks the operator to fall back to the cloud relay. Real bind
/// failures (radio adapter died, tunnel never came up, watchdog fired) count
/// toward this; a flaky bench is not left in a forever-retry loop.
pub const MAX_LOCAL_BIND_ATTEMPTS: u32 = 10;

/// Sidecar file shared with the API process, which exposes the value over a
/// failover-status route. The auto-pair loop and the API run in separate
/// processes, so a file under `/run/ados` bridges them without a new IPC
/// channel. The shape is `{"state": "local" | "cloud_relay"}` and the reader
/// validates the value, so only the key + value strings are load-bearing.
const FAILOVER_STATE_PATH: &str = "/run/ados/wfb_failover.json";

/// Whether auto-pair should attempt a bind this tick. Pure for testing.
pub fn should_attempt(armed: bool, already_paired: bool) -> bool {
    armed && !already_paired
}

/// Decide whether the attempt count has reached the cap that flips the rig from
/// local-bind retries to the cloud-relay fallback. Pure for testing. Mirrors the
/// `attempt >= MAX_LOCAL_BIND_ATTEMPTS` gate: it fires on the Nth consecutive
/// non-paired bind attempt so an operator on a flaky bench is not stuck retrying
/// forever.
pub fn failover_reached(attempt: u32) -> bool {
    attempt >= MAX_LOCAL_BIND_ATTEMPTS
}

/// The two failover states the sidecar can hold.
///
/// `Local` means the rig is still trying to bind over its own radio; `CloudRelay`
/// means it has given up the local loop and asked the operator to fall back to
/// the cloud relay. The wire strings are the only contract the API reader cares
/// about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailoverState {
    Local,
    CloudRelay,
}

impl FailoverState {
    /// The on-disk string. Must match the value the failover-status route
    /// validates against (`local` / `cloud_relay`).
    pub fn as_str(self) -> &'static str {
        match self {
            FailoverState::Local => "local",
            FailoverState::CloudRelay => "cloud_relay",
        }
    }
}

/// Stateful writer for the failover sidecar. Dedups identical syncs (writes on a
/// transition only) and writes atomically (tmp sibling + rename, mode 0o644).
///
/// The body is `{"state": "<value>"}` rendered the same way the Python writer
/// rendered it (two-space indent), so the file is byte-identical regardless of
/// which process last wrote it.
#[derive(Debug)]
pub struct FailoverWriter {
    path: PathBuf,
    /// The last state we persisted, to skip a redundant rewrite. `None` until
    /// the first write so the initial `local` reset always lands.
    last: Option<FailoverState>,
}

impl FailoverWriter {
    /// Writer targeting the canonical sidecar path.
    pub fn new() -> Self {
        Self::with_path(PathBuf::from(FAILOVER_STATE_PATH))
    }

    /// Writer targeting an explicit path (tests).
    pub fn with_path(path: PathBuf) -> Self {
        Self { path, last: None }
    }

    /// Persist `state` if it differs from the last write. Returns `true` when a
    /// write actually touched the disk. A write failure is logged and swallowed
    /// so a transient fs hiccup never crashes the auto-pair loop.
    pub fn sync(&mut self, state: FailoverState) -> bool {
        if self.last == Some(state) {
            return false;
        }
        // Two-space indent + no trailing newline keeps the body byte-identical
        // to the previous writer's output for the same value.
        let body = format!("{{\n  \"state\": \"{}\"\n}}", state.as_str());
        match crate::bind::keys::atomic_write(&self.path, body.as_bytes(), 0o644) {
            Ok(()) => {
                self.last = Some(state);
                true
            }
            Err(exc) => {
                tracing::warn!(error = %exc, state = state.as_str(), "wfb_failover_state_persist_failed");
                false
            }
        }
    }

    /// The path this writer targets.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Default for FailoverWriter {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse `video.wfb.auto_pair_enabled` out of a config body. Default TRUE: a
/// fresh rig auto-binds on first boot unless the operator has explicitly
/// disarmed it, and a successful bind disarms it. An absent `video.wfb` section
/// therefore reads as armed, which also keeps this in step with the Python
/// config model's default (the two diverged before, so the status display read
/// "armed" while the supervisor was silently disarmed).
pub fn read_armed_from(text: &str) -> bool {
    #[derive(serde::Deserialize)]
    struct Raw {
        #[serde(default)]
        video: VideoSec,
    }
    #[derive(serde::Deserialize, Default)]
    struct VideoSec {
        #[serde(default)]
        wfb: Option<WfbSec>,
    }
    #[derive(serde::Deserialize)]
    struct WfbSec {
        #[serde(default = "default_true")]
        auto_pair_enabled: bool,
    }
    fn default_true() -> bool {
        true
    }
    match serde_norway::from_str::<Raw>(text) {
        Ok(raw) => raw.video.wfb.map(|w| w.auto_pair_enabled).unwrap_or(true),
        Err(_) => true,
    }
}

/// Read the arm flag off disk; any read/parse failure keeps the default-armed
/// posture so a transient fs hiccup never silently disables first-boot pairing.
fn read_armed() -> bool {
    match std::fs::read_to_string(CONFIG_YAML) {
        Ok(text) => read_armed_from(&text),
        Err(_) => true,
    }
}

/// True when this role's key file already exists (the rig is paired).
fn already_paired(role: BindRole) -> bool {
    Path::new(role.key_path()).exists()
}

/// Run the auto-pair loop until `shutdown` flips. Drives the bind in-process via
/// the shared orchestrator.
pub async fn run(orch: Arc<BindOrchestrator>, role: BindRole, mut shutdown: watch::Receiver<bool>) {
    run_with_failover(orch, role, &mut shutdown, FailoverWriter::new()).await
}

/// The loop body with an injectable failover writer (so a test can target a temp
/// path). Resets the failover sidecar to `local` on start, drives one bind per
/// armed+unpaired tick, flips to `cloud_relay` after the attempt cap, and persists
/// `local` again on a successful pair.
async fn run_with_failover(
    orch: Arc<BindOrchestrator>,
    role: BindRole,
    shutdown: &mut watch::Receiver<bool>,
    mut failover: FailoverWriter,
) {
    tracing::info!(role = role.as_str(), "auto_pair_supervisor_started");
    // Settle before the first attempt.
    tokio::select! {
        _ = shutdown.changed() => return,
        _ = tokio::time::sleep(START_DELAY) => {}
    }
    // A fresh run resets the failover sidecar to `local`: an operator who re-armed
    // auto-pair from the cloud-relay fallback should see the rig retry local bind.
    failover.sync(FailoverState::Local);
    // Cumulative count of bind attempts; reaching the cap flips to cloud_relay.
    let mut attempt: u32 = 0;
    loop {
        if *shutdown.borrow() {
            break;
        }
        let armed = read_armed();
        let paired = already_paired(role);
        if should_attempt(armed, paired) {
            // Tear down an in-flight bind if the supervisor shuts down.
            let mut cancel_rx = shutdown.clone();
            let cancel = async move {
                let _ = cancel_rx.changed().await;
            };
            match orch.start_local_bind(role, None, "auto", cancel).await {
                Ok(session) => {
                    let state = session.get("state").and_then(|s| s.as_str()).unwrap_or("");
                    if state == "paired" {
                        tracing::info!(role = role.as_str(), "auto_pair_bind_completed");
                        // Stay on the local path; the key-apply step also disarms
                        // auto-pair so the next tick sees the rig as paired.
                        failover.sync(FailoverState::Local);
                        break;
                    }
                    // A non-paired terminal. An external shutdown aborts the bind
                    // and breaks below without counting toward failover; otherwise
                    // this is a real failure worth counting.
                    if *shutdown.borrow() {
                        tracing::info!(role = role.as_str(), state, "auto_pair_aborted");
                        break;
                    }
                    attempt += 1;
                    tracing::info!(
                        role = role.as_str(),
                        state,
                        attempt,
                        "auto_pair_attempt_unpaired"
                    );
                    if failover_reached(attempt) {
                        failover.sync(FailoverState::CloudRelay);
                        tracing::warn!(attempts = attempt, "wfb_failover_to_cloud_relay");
                        break;
                    }
                }
                Err(BindStartError::Busy) => {
                    // Another bind path raced us (REST handler, manual CLI). Defer
                    // without counting toward the failover cap; the busy session
                    // succeeds or fails and the next tick picks up from there.
                    tracing::info!("auto_pair_busy_retry");
                }
            }
        }
        tokio::select! {
            _ = shutdown.changed() => break,
            _ = tokio::time::sleep(RETRY_BACKOFF) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_attempt_gate() {
        assert!(should_attempt(true, false));
        assert!(!should_attempt(false, false)); // disarmed
        assert!(!should_attempt(true, true)); // already paired
    }

    #[test]
    fn armed_defaults_true_without_wfb_section() {
        // A fresh config with no `video.wfb` still arms (first-boot auto-bind).
        assert!(read_armed_from("video:\n  mode: auto\n"));
        assert!(read_armed_from("agent:\n  profile: drone\n"));
        assert!(read_armed_from(""));
    }

    #[test]
    fn armed_honors_explicit_disarm() {
        assert!(!read_armed_from(
            "video:\n  wfb:\n    auto_pair_enabled: false\n"
        ));
        assert!(read_armed_from(
            "video:\n  wfb:\n    auto_pair_enabled: true\n"
        ));
    }

    #[test]
    fn failover_fires_only_at_the_cap() {
        // Below the cap, keep retrying locally.
        for attempt in 0..MAX_LOCAL_BIND_ATTEMPTS {
            assert!(
                !failover_reached(attempt),
                "attempt {attempt} should not fail over"
            );
        }
        // At and past the cap, fall back to the cloud relay.
        assert!(failover_reached(MAX_LOCAL_BIND_ATTEMPTS));
        assert!(failover_reached(MAX_LOCAL_BIND_ATTEMPTS + 1));
    }

    #[test]
    fn failover_state_wire_strings() {
        assert_eq!(FailoverState::Local.as_str(), "local");
        assert_eq!(FailoverState::CloudRelay.as_str(), "cloud_relay");
    }

    #[test]
    fn writer_writes_on_transition_dedups_and_resets() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wfb_failover.json");
        let mut w = FailoverWriter::with_path(path.clone());

        // First reset to local lands (no prior state).
        assert!(w.sync(FailoverState::Local));
        assert!(path.is_file());
        let body = std::fs::read_to_string(&path).unwrap();
        // Body shape the API reader parses: {"state": "local"} (two-space indent).
        assert_eq!(body, "{\n  \"state\": \"local\"\n}");
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["state"], "local");
        // Mode is 0o644 so the API process (different user context) can read it.
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);

        // Same state again → dedup, no write.
        assert!(!w.sync(FailoverState::Local));

        // Transition to cloud_relay → write.
        assert!(w.sync(FailoverState::CloudRelay));
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body, "{\n  \"state\": \"cloud_relay\"\n}");
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["state"], "cloud_relay");

        // Dedup holds at cloud_relay.
        assert!(!w.sync(FailoverState::CloudRelay));

        // Reset back to local (a successful pair) → write.
        assert!(w.sync(FailoverState::Local));
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed["state"], "local");

        // No torn tmp left behind.
        assert!(!dir.path().join("wfb_failover.json.tmp").exists());
    }

    #[test]
    fn attempt_counter_flips_to_cloud_relay_then_resets_on_pair() {
        // Model the loop's decision: count consecutive non-paired attempts, flip
        // the sidecar to cloud_relay at the cap, and reset to local on a pair.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wfb_failover.json");
        let mut w = FailoverWriter::with_path(path.clone());

        // Fresh run resets to local.
        w.sync(FailoverState::Local);

        let mut attempt: u32 = 0;
        let mut flipped = false;
        // Nine consecutive failures: still local, never flipped.
        for _ in 0..(MAX_LOCAL_BIND_ATTEMPTS - 1) {
            attempt += 1;
            if failover_reached(attempt) {
                w.sync(FailoverState::CloudRelay);
                flipped = true;
            }
        }
        assert!(!flipped);
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed["state"], "local");

        // The tenth consecutive failure flips to cloud_relay.
        attempt += 1;
        assert!(failover_reached(attempt));
        w.sync(FailoverState::CloudRelay);
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed["state"], "cloud_relay");

        // A later successful pair resets to local.
        w.sync(FailoverState::Local);
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed["state"], "local");
    }
}
