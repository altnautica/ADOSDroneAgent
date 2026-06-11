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

/// Whether the loop should run a bind given the adapter presence. A dongle-less
/// boot (no injection-capable WFB adapter) must NOT run a bind: the bind would
/// fail and burn one of the limited local-bind attempts, eventually flipping the
/// rig to the cloud-relay fallback even though the operator simply has not
/// plugged the radio in yet. Skipping without counting lets the rig keep waiting
/// for the adapter indefinitely and bind the moment it appears. Pure for testing.
pub fn adapter_ready_to_bind(wfb_adapter_present: bool) -> bool {
    wfb_adapter_present
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

/// True when this role's key file is a real, complete WFB key (the rig is
/// paired). A bare `Path::exists()` treated a half-written key file (a power
/// loss mid-bind, a truncated copy) as paired, so the rig skipped its own
/// auto-bind and never recovered. Reuse the key validator: it succeeds only
/// when the file is exactly 64 bytes (the libsodium crypto_box size) and its
/// peer-public half hashes, which is the same completeness check the bind path
/// applies before it trusts a key.
fn already_paired(role: BindRole) -> bool {
    key_is_complete(Path::new(role.key_path()))
}

/// True when the key file at `path` passes the WFB key validator (64-byte
/// length + fingerprintable peer-public half). Split out so the completeness
/// check is unit-testable against a temp file without a real role key path.
fn key_is_complete(path: &Path) -> bool {
    crate::bind::keys::read_public_fingerprint(path).is_ok()
}

/// Run the auto-pair loop until `shutdown` flips. Drives the bind in-process via
/// the shared orchestrator. `cloud_relay_enabled` reflects `server.mode`: in
/// local-first mode the loop never gives up on the local bind.
pub async fn run(
    orch: Arc<BindOrchestrator>,
    role: BindRole,
    mut shutdown: watch::Receiver<bool>,
    cloud_relay_enabled: bool,
) {
    run_with_failover(
        orch,
        role,
        &mut shutdown,
        FailoverWriter::new(),
        cloud_relay_enabled,
    )
    .await
}

/// The loop body with an injectable failover writer (so a test can target a temp
/// path). Resets the failover sidecar to `local` on start, drives one bind per
/// armed+unpaired tick, flips to `cloud_relay` after the attempt cap *only when
/// a cloud relay is configured*, and persists `local` again on a successful pair.
async fn run_with_failover(
    orch: Arc<BindOrchestrator>,
    role: BindRole,
    shutdown: &mut watch::Receiver<bool>,
    mut failover: FailoverWriter,
    cloud_relay_enabled: bool,
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
        // A dongle-less boot must not run (and fail) a bind: that would burn the
        // limited local-bind attempts and flip to cloud_relay when the operator
        // has simply not plugged the radio in yet. Skip without counting so the
        // rig keeps waiting and binds the moment the adapter appears.
        let adapter_present = adapter_ready_to_bind(crate::hardware::has_wfb_adapter());
        if should_attempt(armed, paired) && !adapter_present {
            tracing::info!(role = role.as_str(), "auto_pair_waiting_for_wfb_adapter");
        }
        if should_attempt(armed, paired) && adapter_present {
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
                    attempt = attempt.saturating_add(1);
                    tracing::info!(
                        role = role.as_str(),
                        state,
                        attempt,
                        "auto_pair_attempt_unpaired"
                    );
                    // Fail over to the cloud relay only when it is actually
                    // configured (`server.mode` = cloud / self_hosted). In
                    // local-first / offline operation WFB is the only link, so
                    // the loop keeps retrying the local bind forever instead of
                    // giving up at the attempt cap and stranding the rig with no
                    // link at all.
                    if cloud_relay_enabled && failover_reached(attempt) {
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
    fn key_completeness_rejects_half_written_key() {
        // A complete 64-byte key counts as paired; a short (half-written, e.g.
        // power loss mid-bind) or absent key does NOT, so the rig re-runs its
        // own auto-bind instead of skipping forever on a truncated file.
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("drone.key");
        std::fs::write(&good, [7u8; 64]).unwrap();
        assert!(key_is_complete(&good));

        let half = dir.path().join("half.key");
        std::fs::write(&half, [7u8; 32]).unwrap(); // truncated write
        assert!(!key_is_complete(&half));

        let empty = dir.path().join("empty.key");
        std::fs::write(&empty, []).unwrap();
        assert!(!key_is_complete(&empty));

        // A path that does not exist is not paired.
        assert!(!key_is_complete(&dir.path().join("missing.key")));
    }

    #[test]
    fn adapter_gate_skips_bind_without_an_adapter() {
        // With an injection adapter present, a bind may run; without one it must
        // not (the gate is what keeps a dongle-less boot from burning attempts).
        assert!(adapter_ready_to_bind(true));
        assert!(!adapter_ready_to_bind(false));
    }

    #[test]
    fn no_adapter_does_not_consume_a_failover_attempt() {
        // Model the loop's decision: when armed + unpaired but no adapter is
        // present, the bind is skipped and the attempt counter is untouched, so
        // a dongle-less rig never flips to cloud_relay while it waits.
        let armed = true;
        let paired = false;
        let mut attempt: u32 = 0;
        for _ in 0..(MAX_LOCAL_BIND_ATTEMPTS + 5) {
            let adapter_present = adapter_ready_to_bind(false); // never plugged in
            if should_attempt(armed, paired) && adapter_present {
                attempt += 1; // the only path that counts toward failover
            }
        }
        assert_eq!(attempt, 0);
        assert!(!failover_reached(attempt));
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
    fn local_mode_never_fails_over_to_cloud_relay() {
        // In local-first mode (server.mode = local → cloud_relay_enabled =
        // false) the loop's failover gate never fires past the cap, so an
        // offline rig keeps retrying the local bind instead of giving up.
        let cloud_relay_enabled = false;
        let mut flipped = false;
        for attempt in 1..=(MAX_LOCAL_BIND_ATTEMPTS + 5) {
            if cloud_relay_enabled && failover_reached(attempt) {
                flipped = true;
                break;
            }
        }
        assert!(!flipped, "local mode must never give up the WFB bind");
    }

    #[test]
    fn cloud_mode_fails_over_at_the_cap() {
        // With a cloud relay configured, the same gate flips at the cap.
        let cloud_relay_enabled = true;
        let mut flipped_at = None;
        for attempt in 1..=(MAX_LOCAL_BIND_ATTEMPTS + 5) {
            if cloud_relay_enabled && failover_reached(attempt) {
                flipped_at = Some(attempt);
                break;
            }
        }
        assert_eq!(flipped_at, Some(MAX_LOCAL_BIND_ATTEMPTS));
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
