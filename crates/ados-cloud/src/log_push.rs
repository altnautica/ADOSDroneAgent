//! Explicit, account-gated cloud export of a chosen log window.
//!
//! The durable on-device log store is the single source of truth. This module
//! adds the only path that copies a chosen window of it to the paired cloud
//! account, and it is deliberately conservative:
//!
//! - **Explicit.** A push runs only when an operator-triggered request lands as
//!   a small JSON file at [`PUSH_REQUEST_PATH`]. There is no continuous firehose
//!   and no auto-call from any periodic loop.
//! - **Account-gated, default off.** A push runs only when the agent is
//!   cloud-paired (a live api key + a non-empty cloud url) AND the operator has
//!   turned on the `server.cloud_logs_enabled` opt-in. An unpaired or local-only
//!   agent logging fully to disk with nothing exported is correct, not broken.
//! - **Idempotent.** The cloud dedups on the server-recomputed hash of the
//!   uploaded bytes, so a re-push of the same deterministic window is a no-op
//!   that still marks the rows exported.
//!
//! The push sequence for each requested kind, mirroring the build contract:
//!
//! 1. GET the unsynced window from the logging daemon over its trusted local
//!    socket (`/v1/export?...&unsynced=1`), spooling the bytes and computing
//!    their hash and size. A window over [`MAX_WINDOW_BYTES`] is split at its
//!    time midpoint into consecutive sub-windows, earliest first, until each
//!    fits; every step below then runs per sub-window.
//! 2. POST those exact bytes to the cloud ingest route in one authenticated
//!    binary request (the same device-api-key header auth the heartbeat uses).
//! 3. On a 2xx that is not a duplicate, POST `/v1/synced` over the same local
//!    socket to flip the exported rows to synced — the same window, so the next
//!    push never re-uploads them.
//!
//! The result of the whole request is written to [`PUSH_RESULT_PATH`] for the
//! trigger front end to read back, and the request file is removed when done.
//! Both files live under the runtime dir, the same bridge other cross-process
//! signals use.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::CloudConfig;
use crate::pairing::PairingState;

mod export;
use export::*;

/// The trigger file the front end writes to request a push. Removed when the
/// request has been serviced.
pub const PUSH_REQUEST_PATH: &str = "/run/ados/logd-push-request.json";

/// The result file the watcher writes after servicing a request.
pub const PUSH_RESULT_PATH: &str = "/run/ados/logd-push-result.json";

/// The logging daemon's trusted local query/control socket.
pub const LOGD_QUERY_SOCK: &str = "/run/ados/logd-query.sock";

/// How often the watcher polls for a new request file.
pub const WATCH_INTERVAL: Duration = Duration::from_secs(2);

/// The per-window upload cap. One upload is bounded so a constrained uplink is
/// never flooded, and the response buffer stays small on a 1 GB board; a larger
/// backlog is sent as several consecutive windows.
pub const MAX_WINDOW_BYTES: usize = 8 * 1024 * 1024;

/// Slack over [`MAX_WINDOW_BYTES`] for the response framing (status line,
/// headers, and chunk-size lines) when bounding the socket read. The export body
/// is the bulk; this keeps a legitimate at-cap window from being rejected by the
/// few hundred bytes of HTTP framing while still bounding the read well before a
/// runaway body can exhaust memory on a small board.
const READ_SLACK_BYTES: usize = 64 * 1024;

/// The hard ceiling on bytes read from the local socket for one request, so a
/// multi-day unsynced backlog cannot be buffered into memory before the cap is
/// checked. The read is aborted the moment this is exceeded.
const MAX_READ_BYTES: usize = MAX_WINDOW_BYTES + READ_SLACK_BYTES;

/// The valid export kinds. One kind maps to one logging-store table; a push of
/// several kinds is several windows.
const ALL_KINDS: [&str; 4] = ["logs", "metrics", "events", "hw"];

/// A push request as written to [`PUSH_REQUEST_PATH`]. Every field is optional:
/// a bare `{}` exports the unsynced rows of every kind with no time floor.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PushRequest {
    /// Restrict to one session id, or all sessions when absent.
    #[serde(default)]
    pub session: Option<i64>,
    /// Lower time bound in epoch-microseconds, or no floor when absent. The
    /// upper bound is captured by the watcher at service time so the export and
    /// the mark cover the identical, deterministic window.
    #[serde(default)]
    pub since_us: Option<i64>,
    /// The kinds to export. Empty/absent means all four.
    #[serde(default)]
    pub kinds: Vec<String>,
}

impl PushRequest {
    /// The requested kinds normalized to the known set, deduplicated and in a
    /// stable order. An empty/absent list means all four.
    pub fn resolved_kinds(&self) -> Vec<String> {
        if self.kinds.is_empty() {
            return ALL_KINDS.iter().map(|k| k.to_string()).collect();
        }
        let wanted: BTreeSet<&str> = self.kinds.iter().map(|k| k.as_str()).collect();
        ALL_KINDS
            .iter()
            .filter(|k| wanted.contains(*k))
            .map(|k| k.to_string())
            .collect()
    }
}

/// The outcome of one push request, written to [`PUSH_RESULT_PATH`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PushResult {
    /// How many windows were freshly stored in the cloud account.
    pub pushed: u32,
    /// How many windows the cloud already had (deduped, no re-store).
    pub deduped: u32,
    /// Total uploaded bytes across the pushed (non-deduped) windows.
    pub bytes: u64,
    /// A terminal error, when the whole request could not run (gate failure or
    /// transport failure). A per-kind failure is logged and skipped, not fatal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl PushResult {
    /// A result that failed before any window could be pushed (e.g. the gate
    /// rejected the request).
    pub fn errored(code: &str) -> Self {
        PushResult {
            error: Some(code.to_string()),
            ..Default::default()
        }
    }
}

/// Why a push was refused. The codes are stable, behaviour-describing strings
/// the front end maps to a status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateError {
    /// The relay is in local mode or the agent is not cloud-paired.
    NotCloudPaired,
    /// The operator has not turned on the cloud-logs opt-in.
    CloudLogsDisabled,
    /// No cloud backend url is configured.
    NoBackend,
}

impl GateError {
    pub fn code(&self) -> &'static str {
        match self {
            GateError::NotCloudPaired => "not_cloud_paired",
            GateError::CloudLogsDisabled => "cloud_logs_disabled",
            GateError::NoBackend => "no_backend",
        }
    }
}

/// The resolved gate inputs. Splitting the decision out of the IO lets the gate
/// be unit-tested without a config file or a paired state on disk.
#[derive(Debug, Clone)]
pub struct PushGate<'a> {
    pub mode: &'a str,
    pub cloud_logs_enabled: bool,
    pub api_key: Option<&'a str>,
    pub convex_url: &'a str,
}

impl<'a> PushGate<'a> {
    /// Resolve the gate from the live config + pairing state. Returns the api key
    /// and the cloud url to use when the gate passes.
    pub fn check(&self) -> Result<(&'a str, &'a str), GateError> {
        if self.mode == "local" || self.convex_url.is_empty() {
            // A local-only agent is correct, not misconfigured.
            return Err(GateError::NotCloudPaired);
        }
        if !self.cloud_logs_enabled {
            return Err(GateError::CloudLogsDisabled);
        }
        let Some(key) = self.api_key else {
            return Err(GateError::NotCloudPaired);
        };
        if key.is_empty() {
            return Err(GateError::NotCloudPaired);
        }
        if self.convex_url.is_empty() {
            return Err(GateError::NoBackend);
        }
        Ok((key, self.convex_url))
    }
}

/// The decision the cloud ingest response drives: did we store a new window, was
/// it a duplicate the cloud already had, and should we now mark the rows synced?
///
/// The mark runs on a fresh store AND on a duplicate (a duplicate means the
/// window is already in the cloud, so the local rows are safe to flip), but
/// never when the upload itself failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IngestDecision {
    pub deduped: bool,
    pub mark_synced: bool,
}

impl IngestDecision {
    /// Map the cloud ingest `status` field to the local decision.
    /// `inserted` → store + mark; `duplicate` → mark only (no new store).
    pub fn from_status(status: &str) -> Self {
        let deduped = status == "duplicate";
        IngestDecision {
            deduped,
            // Both a fresh insert and a duplicate are safe to mark: in either
            // case the cloud holds the window.
            mark_synced: status == "inserted" || status == "duplicate",
        }
    }
}

/// Compute the lowercase-hex SHA-256 of the window bytes. This is the same
/// content hash the cloud recomputes server-side; matching it locally lets the
/// result report the exported window's identity.
pub fn window_hash(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

/// Count the rows in a `jsonl.zst` window: the export writes one record per line
/// terminated by `\n`, so the number of newlines in the decompressed stream is
/// the exact row count. The decode streams through a fixed buffer so the count
/// uses constant memory regardless of the decompressed size, and a corrupt /
/// truncated stream yields the count of whatever decoded cleanly rather than a
/// failure (the count is metadata, never a gate on the upload).
pub fn count_jsonl_rows(zst: &[u8]) -> u64 {
    use std::io::Read;
    let mut decoder = match zstd::stream::read::Decoder::new(zst) {
        Ok(d) => d,
        Err(_) => return 0,
    };
    let mut rows: u64 = 0;
    let mut buf = [0u8; 64 * 1024];
    loop {
        match decoder.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => rows += bytecount_newlines(&buf[..n]),
            // A partial / corrupt frame ends the count at what decoded so far.
            Err(_) => break,
        }
    }
    rows
}

/// Count `\n` bytes in a slice.
fn bytecount_newlines(slice: &[u8]) -> u64 {
    slice.iter().filter(|&&b| b == b'\n').count() as u64
}

/// Current wall-clock in epoch-microseconds. Used as the closed upper bound of
/// the export+mark window so a retry exports byte-identical rows.
fn now_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

/// Spawn the request-file watcher. It polls [`PUSH_REQUEST_PATH`] on
/// [`WATCH_INTERVAL`]; on a request it services the push and writes the result.
/// The config + pairing state are re-read per request so a pair/unpair or an
/// opt-in toggle is observed without a restart. Best-effort throughout: a bad
/// request file or a transport failure is recorded in the result, never fatal.
pub fn spawn_log_push_watcher(
    config: std::sync::Arc<CloudConfig>,
    http: std::sync::Arc<reqwest::Client>,
    convex_url: String,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(WATCH_INTERVAL);
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { break; }
                }
                _ = tick.tick() => {
                    if !Path::new(PUSH_REQUEST_PATH).exists() {
                        continue;
                    }
                    let result = service_request_file(
                        &config,
                        &http,
                        &convex_url,
                        Path::new(PUSH_REQUEST_PATH),
                    )
                    .await;
                    write_result(Path::new(PUSH_RESULT_PATH), &result);
                    // Always remove the request so it is serviced once. A failed
                    // push leaves the rows unsynced; the operator may retry.
                    let _ = std::fs::remove_file(PUSH_REQUEST_PATH);
                }
            }
        }
    })
}

/// Read + parse a request file, then run the push. A missing or unparseable file
/// yields a recorded error rather than a panic.
async fn service_request_file(
    config: &CloudConfig,
    http: &reqwest::Client,
    convex_url: &str,
    request_path: &Path,
) -> PushResult {
    let req: PushRequest = match std::fs::read_to_string(request_path) {
        Ok(text) => match serde_json::from_str(&text) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "log-push request file is not valid json");
                return PushResult::errored("bad_request");
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, "could not read the log-push request file");
            return PushResult::errored("bad_request");
        }
    };
    let pairing = PairingState::load();
    run_push(config, &pairing, http, convex_url, &req).await
}

/// Run a push for one parsed request: gate, then loop the requested kinds,
/// exporting each kind's unsynced rows over the local socket in windows that
/// fit the upload cap, uploading each, and marking each on success. Aggregates
/// the per-kind outcomes into one [`PushResult`].
pub async fn run_push(
    config: &CloudConfig,
    pairing: &PairingState,
    http: &reqwest::Client,
    convex_url: &str,
    req: &PushRequest,
) -> PushResult {
    let gate = PushGate {
        mode: config.server.mode.as_str(),
        cloud_logs_enabled: config.cloud_logs_enabled(),
        api_key: pairing.api_key(),
        convex_url,
    };
    let (api_key, base_url) = match gate.check() {
        Ok(pair) => pair,
        Err(e) => {
            tracing::info!(reason = e.code(), "log-push refused");
            return PushResult::errored(e.code());
        }
    };

    let device_id = config.agent.device_id.as_str();
    // A single closed upper bound for every kind in this request, so the export
    // and the mark cover the identical deterministic window across a retry.
    let to_us = now_us();

    let mut out = PushResult::default();
    for kind in req.resolved_kinds() {
        let mut sink = LiveWindowSink {
            http,
            base_url,
            api_key,
            device_id,
            kind: &kind,
            session: req.session,
            socket: Path::new(LOGD_QUERY_SOCK),
        };
        match push_kind_windows(&mut sink, req.since_us, to_us).await {
            Ok(k) => {
                out.pushed += k.pushed;
                out.deduped += k.deduped;
                out.bytes += k.bytes;
            }
            Err(e) => {
                tracing::warn!(kind = %kind, error = %e, "log-push for one kind failed");
                // Record the first terminal error but keep trying the rest so a
                // single empty/failed kind does not block the others.
                if out.error.is_none() {
                    out.error = Some(e);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests;
