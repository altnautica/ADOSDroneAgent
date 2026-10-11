//! The `wfb_failover.json` sidecar: the local/cloud-relay failover state the
//! auto-pair loop publishes for the API process.

use std::path::{Path, PathBuf};

use ados_protocol::logd::emitter::EventEmitter;
use ados_protocol::logd::{Fields, Level, Value};

/// Sidecar file shared with the API process, which exposes the value over a
/// failover-status route. The auto-pair loop and the API run in separate
/// processes, so a file under `/run/ados` bridges them without a new IPC
/// channel. The shape is `{"state": "local" | "cloud_relay"}` and the reader
/// validates the value, so only the key + value strings are load-bearing.
const FAILOVER_STATE_PATH: &str = "/run/ados/wfb_failover.json";

/// Schema version of the `wfb_failover.json` sidecar. Bump on an incompatible
/// field-set change; a reader compares it best-effort via
/// `ados_protocol::sidecar::check_sidecar_version`. Kept in step with the
/// registry in `contracts.toml`.
pub(super) const WFB_FAILOVER_SIDECAR_VERSION: u16 = 1;

/// The two failover states the sidecar can hold.
///
/// `Local` means the rig is binding over its own radio; `CloudRelay` means its
/// local attempts have kept failing and it is telling the operator to fall back
/// to the cloud relay — while it keeps trying locally. The wire strings are the
/// only contract the API reader cares about.
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

/// Ship one local-bind to cloud-relay failover transition to the logging store
/// as a discrete event, so an RCA can query the transition boundaries the same
/// way it queries the sidecar's current value. Called only when the sidecar
/// actually changed (a deduped no-op write emits nothing). A cloud-relay fallback
/// is a degraded condition (`Warn`); returning to local bind is informational
/// (`Info`). Best-effort: an absent logging daemon drops the event.
pub(super) fn emit_failover(events: &EventEmitter, state: FailoverState) {
    let mut detail = Fields::new();
    detail.insert("state".to_string(), Value::from(state.as_str()));
    let level = match state {
        FailoverState::CloudRelay => Level::Warn,
        FailoverState::Local => Level::Info,
    };
    events.emit("wfb.pair.failover", level, detail);
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
        // Two-space indent + no trailing newline, matching the prior writer's
        // shape. The additive `version` field is a best-effort schema-drift
        // signal for readers; `state` stays the load-bearing value.
        let body = format!(
            "{{\n  \"version\": {},\n  \"state\": \"{}\"\n}}",
            WFB_FAILOVER_SIDECAR_VERSION,
            state.as_str()
        );
        match ados_protocol::sidecar::write_atomic(&self.path, body.as_bytes(), 0o644) {
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
