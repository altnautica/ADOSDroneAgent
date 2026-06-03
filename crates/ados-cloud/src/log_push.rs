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
//!    their hash and size.
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

/// The trigger file the front end writes to request a push. Removed when the
/// request has been serviced.
pub const PUSH_REQUEST_PATH: &str = "/run/ados/logd-push-request.json";

/// The result file the watcher writes after servicing a request.
pub const PUSH_RESULT_PATH: &str = "/run/ados/logd-push-result.json";

/// The logging daemon's trusted local query/control socket.
pub const LOGD_QUERY_SOCK: &str = "/run/ados/logd-query.sock";

/// How often the watcher polls for a new request file.
pub const WATCH_INTERVAL: Duration = Duration::from_secs(2);

/// The per-window upload cap. A single explicit window is bounded; a body over
/// this is refused before any upload so a constrained uplink is never flooded.
pub const MAX_WINDOW_BYTES: usize = 32 * 1024 * 1024;

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
/// exporting each window over the local socket, uploading it, and marking the
/// rows on success. Aggregates the per-kind outcomes into one [`PushResult`].
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
        match push_one_kind(
            http,
            base_url,
            api_key,
            device_id,
            &kind,
            req.session,
            req.since_us,
            to_us,
        )
        .await
        {
            Ok(KindOutcome { deduped, bytes }) => {
                if deduped {
                    out.deduped += 1;
                } else {
                    out.pushed += 1;
                    out.bytes += bytes as u64;
                }
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

/// One kind's outcome.
struct KindOutcome {
    deduped: bool,
    bytes: usize,
}

/// Export, upload, and (on success) mark one kind's window. Returns the outcome
/// or a terminal-error code string for this kind.
#[allow(clippy::too_many_arguments)]
async fn push_one_kind(
    http: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    device_id: &str,
    kind: &str,
    session: Option<i64>,
    since_us: Option<i64>,
    to_us: i64,
) -> Result<KindOutcome, String> {
    // 1. Export the unsynced window over the trusted local socket.
    let bytes = export_window(Path::new(LOGD_QUERY_SOCK), kind, session, since_us, to_us)
        .await
        .map_err(|_| "store_unreachable".to_string())?;
    if bytes.is_empty() {
        return Err("empty_window".to_string());
    }
    if bytes.len() > MAX_WINDOW_BYTES {
        return Err("window_too_large".to_string());
    }
    let content_hash = window_hash(&bytes);
    let size = bytes.len();

    // 2. Upload to the cloud ingest route in one authenticated binary POST. The
    //    cloud recomputes the hash server-side; the local hash only labels the
    //    result.
    let url = format!("{}/agent/logd/window", base_url.trim_end_matches('/'));
    let resp = http
        .post(&url)
        .header("X-ADOS-Key", api_key)
        .header("X-ADOS-Device", device_id)
        .header(
            "X-ADOS-Session",
            session.map(|s| s.to_string()).unwrap_or_default(),
        )
        .header("X-ADOS-Kind", kind)
        .header("X-ADOS-Format", "jsonl.zst")
        .header("X-ADOS-Window-Start-Us", since_us.unwrap_or(0).to_string())
        .header("X-ADOS-Window-End-Us", to_us.to_string())
        .header("X-ADOS-Row-Count", "0")
        .header("Content-Type", "application/zstd")
        .body(bytes)
        .send()
        .await
        .map_err(|_| "cloud_error".to_string())?;
    if !resp.status().is_success() {
        return Err("cloud_error".to_string());
    }
    let status_field = resp
        .json::<serde_json::Value>()
        .await
        .ok()
        .and_then(|v| v.get("status").and_then(|s| s.as_str()).map(str::to_string))
        .unwrap_or_else(|| "inserted".to_string());
    let decision = IngestDecision::from_status(&status_field);
    tracing::info!(
        kind = %kind, deduped = decision.deduped, hash = %content_hash, bytes = size,
        "log window uploaded"
    );

    // 3. Mark the exact window synced over the local socket — only on a stored
    //    (or already-present) window. A mark failure is non-fatal: the cloud
    //    copy exists, and a later push re-dedupes and re-marks.
    if decision.mark_synced {
        if let Err(e) =
            mark_synced(Path::new(LOGD_QUERY_SOCK), kind, session, since_us, to_us).await
        {
            tracing::warn!(kind = %kind, error = %e, "mark-synced failed after upload");
        }
    }

    Ok(KindOutcome {
        deduped: decision.deduped,
        bytes: size,
    })
}

/// GET the unsynced export window from the logging daemon over its local socket,
/// returning the raw `jsonl.zst` bytes. The window is `[since_us, to_us)`,
/// optionally scoped to one session, restricted to the rows not yet marked
/// synced — byte-identical to what the upload sends.
async fn export_window(
    socket: &Path,
    kind: &str,
    session: Option<i64>,
    since_us: Option<i64>,
    to_us: i64,
) -> std::io::Result<Vec<u8>> {
    let mut query = format!("kind={kind}&unsynced=1&format=jsonl.zst&to={to_us}");
    if let Some(s) = session {
        query.push_str(&format!("&session={s}"));
    }
    if let Some(f) = since_us {
        query.push_str(&format!("&from={f}"));
    }
    let path = format!("/v1/export?{query}");
    let (status, body) = uds_request(socket, "GET", &path, None).await?;
    if !(200..300).contains(&status) {
        return Err(std::io::Error::other(format!(
            "logd export returned status {status}"
        )));
    }
    Ok(body)
}

/// POST `/v1/synced` over the local socket to flip the exported rows to synced.
/// The marked window MUST equal the exported window, so the same selector is
/// used: the session, the lower bound, and the closed upper bound.
async fn mark_synced(
    socket: &Path,
    kind: &str,
    session: Option<i64>,
    since_us: Option<i64>,
    to_us: i64,
) -> std::io::Result<()> {
    let body = serde_json::json!({
        "session": session,
        "from_us": since_us,
        "to_us": to_us,
        "tables": [kind],
    });
    let body_bytes = serde_json::to_vec(&body).unwrap_or_default();
    let (status, _resp) = uds_request(socket, "POST", "/v1/synced", Some(&body_bytes)).await?;
    if !(200..300).contains(&status) {
        return Err(std::io::Error::other(format!(
            "logd mark-synced returned status {status}"
        )));
    }
    Ok(())
}

/// A minimal binary-safe HTTP/1.1 request over a Unix socket. The logging daemon
/// is reached only on its trusted local socket (the mark endpoint is socket-only
/// by design), and the export body is binary `jsonl.zst`, so a byte-safe reader
/// is used rather than a string one. `Connection: close` lets the body be read
/// to EOF; a chunked body is de-chunked.
async fn uds_request(
    socket: &Path,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
) -> std::io::Result<(u16, Vec<u8>)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = tokio::net::UnixStream::connect(socket).await?;
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
    if let Some(b) = body {
        head.push_str("Content-Type: application/json\r\n");
        head.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    if let Some(b) = body {
        stream.write_all(b).await?;
    }
    stream.flush().await?;

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await?;
    parse_http_response(&raw)
}

/// Split a raw HTTP/1.1 response into the status code and the decoded body bytes.
/// De-chunks a `Transfer-Encoding: chunked` body; otherwise returns the body
/// after the header terminator as-is.
fn parse_http_response(raw: &[u8]) -> std::io::Result<(u16, Vec<u8>)> {
    let sep = b"\r\n\r\n";
    let split = raw
        .windows(sep.len())
        .position(|w| w == sep)
        .ok_or_else(|| std::io::Error::other("malformed http response (no header terminator)"))?;
    let head = &raw[..split];
    let body = &raw[split + sep.len()..];

    let head_str = String::from_utf8_lossy(head);
    let status = head_str
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| std::io::Error::other("malformed http status line"))?;

    let chunked = head_str
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked");
    let body = if chunked {
        de_chunk(body)
    } else {
        body.to_vec()
    };
    Ok((status, body))
}

/// De-chunk a `Transfer-Encoding: chunked` body byte-safely:
/// `<hexlen>\r\n<data>\r\n` repeated until a zero-length chunk.
fn de_chunk(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rest = body;
    // The chunk-size line ends at the first CRLF; an absent CRLF ends the body.
    while let Some(crlf) = rest.windows(2).position(|w| w == b"\r\n") {
        let len_line = &rest[..crlf];
        let len = usize::from_str_radix(String::from_utf8_lossy(len_line).trim(), 16).unwrap_or(0);
        if len == 0 {
            break;
        }
        let data_start = crlf + 2;
        if rest.len() < data_start + len {
            // Truncated; take what is present and stop.
            out.extend_from_slice(&rest[data_start..]);
            break;
        }
        out.extend_from_slice(&rest[data_start..data_start + len]);
        // Skip the trailing CRLF after the chunk data.
        let next = data_start + len;
        rest = if rest.len() >= next + 2 {
            &rest[next + 2..]
        } else {
            &[]
        };
    }
    out
}

/// Write the result file atomically (write a temp sibling, then rename) so a
/// reader never sees a partial file.
fn write_result(path: &Path, result: &PushResult) {
    let Ok(json) = serde_json::to_vec(result) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, &json).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── request / result (de)serialization ──────────────────────────────

    #[test]
    fn push_request_round_trips_and_omits_absent_fields() {
        let req = PushRequest {
            session: Some(7),
            since_us: Some(1_700_000_000_000_000),
            kinds: vec!["logs".to_string(), "events".to_string()],
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: PushRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, back);

        // A bare object parses to all-defaults.
        let bare: PushRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(bare, PushRequest::default());

        // Null fields parse as None / empty.
        let nulls: PushRequest =
            serde_json::from_str(r#"{"session":null,"since_us":null,"kinds":[]}"#).unwrap();
        assert_eq!(nulls, PushRequest::default());
    }

    #[test]
    fn resolved_kinds_defaults_to_all_and_filters_unknowns() {
        // Empty → all four in stable order.
        assert_eq!(
            PushRequest::default().resolved_kinds(),
            vec!["logs", "metrics", "events", "hw"]
        );
        // A subset, deduped and ordered, with an unknown kind dropped.
        let req = PushRequest {
            kinds: vec![
                "events".to_string(),
                "logs".to_string(),
                "events".to_string(),
                "bogus".to_string(),
            ],
            ..Default::default()
        };
        assert_eq!(req.resolved_kinds(), vec!["logs", "events"]);
    }

    #[test]
    fn push_result_serializes_without_error_when_clean() {
        let ok = PushResult {
            pushed: 2,
            deduped: 1,
            bytes: 4096,
            error: None,
        };
        let json = serde_json::to_string(&ok).unwrap();
        // The `error` key is omitted when absent.
        assert!(!json.contains("error"));
        let back: PushResult = serde_json::from_str(&json).unwrap();
        assert_eq!(ok, back);

        let bad = PushResult::errored("not_cloud_paired");
        let json = serde_json::to_string(&bad).unwrap();
        assert!(json.contains("not_cloud_paired"));
        let back: PushResult = serde_json::from_str(&json).unwrap();
        assert_eq!(bad, back);
    }

    // ── sha256 / idempotency decision ───────────────────────────────────

    #[test]
    fn window_hash_is_stable_lowercase_hex_sha256() {
        // The empty SHA-256 digest, lowercase hex — the canonical anchor.
        assert_eq!(
            window_hash(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // Deterministic and content-sensitive.
        let a = window_hash(b"the same window");
        let b = window_hash(b"the same window");
        let c = window_hash(b"a different window");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 64);
        assert!(a
            .chars()
            .all(|ch| ch.is_ascii_hexdigit() && !ch.is_ascii_uppercase()));
    }

    #[test]
    fn ingest_decision_inserted_stores_and_marks() {
        let d = IngestDecision::from_status("inserted");
        assert!(!d.deduped);
        assert!(d.mark_synced);
    }

    #[test]
    fn ingest_decision_duplicate_marks_without_storing() {
        // A duplicate means the cloud already holds the window, so the local
        // rows are still safe to flip to synced — but no new store happened.
        let d = IngestDecision::from_status("duplicate");
        assert!(d.deduped);
        assert!(d.mark_synced);
    }

    #[test]
    fn ingest_decision_unknown_status_does_not_mark() {
        let d = IngestDecision::from_status("rejected");
        assert!(!d.deduped);
        assert!(!d.mark_synced);
    }

    // ── the gate ────────────────────────────────────────────────────────

    fn passing_gate<'a>() -> PushGate<'a> {
        PushGate {
            mode: "cloud",
            cloud_logs_enabled: true,
            api_key: Some("k-123"),
            convex_url: "https://relay.example/convex",
        }
    }

    #[test]
    fn gate_passes_only_when_paired_enabled_and_keyed() {
        let (key, url) = passing_gate().check().unwrap();
        assert_eq!(key, "k-123");
        assert_eq!(url, "https://relay.example/convex");
    }

    #[test]
    fn gate_rejects_local_mode() {
        let g = PushGate {
            mode: "local",
            ..passing_gate()
        };
        assert_eq!(g.check().unwrap_err(), GateError::NotCloudPaired);
    }

    #[test]
    fn gate_rejects_empty_convex_url() {
        let g = PushGate {
            convex_url: "",
            ..passing_gate()
        };
        assert_eq!(g.check().unwrap_err(), GateError::NotCloudPaired);
    }

    #[test]
    fn gate_rejects_when_opt_in_off() {
        let g = PushGate {
            cloud_logs_enabled: false,
            ..passing_gate()
        };
        assert_eq!(g.check().unwrap_err(), GateError::CloudLogsDisabled);
    }

    #[test]
    fn gate_rejects_when_unpaired_no_key() {
        let g = PushGate {
            api_key: None,
            ..passing_gate()
        };
        assert_eq!(g.check().unwrap_err(), GateError::NotCloudPaired);

        let g = PushGate {
            api_key: Some(""),
            ..passing_gate()
        };
        assert_eq!(g.check().unwrap_err(), GateError::NotCloudPaired);
    }

    #[test]
    fn gate_error_codes_are_stable_behaviour_strings() {
        assert_eq!(GateError::NotCloudPaired.code(), "not_cloud_paired");
        assert_eq!(GateError::CloudLogsDisabled.code(), "cloud_logs_disabled");
        assert_eq!(GateError::NoBackend.code(), "no_backend");
    }

    // ── run_push gate short-circuit (no transport built when refused) ────

    /// An http client built on the crate's preconfigured TLS, the same one the
    /// daemon uses. A bare `reqwest::Client::new()` panics under this crate's
    /// no-default-provider TLS feature set, so the preconfigured config is used.
    /// The gate short-circuits before any request is sent, so no network is hit.
    fn test_client() -> reqwest::Client {
        reqwest::Client::builder()
            .use_preconfigured_tls(crate::tls::client_config())
            .build()
            .expect("reqwest builds with the rustls config")
    }

    #[tokio::test]
    async fn run_push_refuses_unpaired_before_any_transport() {
        let mut config = CloudConfig::default();
        config.server.mode = "cloud".to_string();
        config.server.cloud_logs_enabled = true;
        // Unpaired pairing state → no api key.
        let pairing = PairingState::default();
        let http = test_client();
        let req = PushRequest::default();
        // An unreachable url proves the gate short-circuits before any POST.
        let res = run_push(&config, &pairing, &http, "https://127.0.0.1:1/convex", &req).await;
        assert_eq!(res.error.as_deref(), Some("not_cloud_paired"));
        assert_eq!(res.pushed, 0);
        assert_eq!(res.deduped, 0);
    }

    #[tokio::test]
    async fn run_push_refuses_when_opt_in_off() {
        let mut config = CloudConfig::default();
        config.server.mode = "cloud".to_string();
        config.server.cloud_logs_enabled = false; // opt-in off
        let pairing = paired_state();
        let http = test_client();
        let res = run_push(
            &config,
            &pairing,
            &http,
            "https://127.0.0.1:1/convex",
            &PushRequest::default(),
        )
        .await;
        assert_eq!(res.error.as_deref(), Some("cloud_logs_disabled"));
    }

    #[tokio::test]
    async fn run_push_refuses_in_local_mode() {
        let mut config = CloudConfig::default();
        config.server.mode = "local".to_string();
        config.server.cloud_logs_enabled = true;
        let pairing = paired_state();
        let http = test_client();
        let res = run_push(
            &config,
            &pairing,
            &http,
            "https://127.0.0.1:1/convex",
            &PushRequest::default(),
        )
        .await;
        assert_eq!(res.error.as_deref(), Some("not_cloud_paired"));
    }

    /// A paired state with a live api key (built via the on-disk reader so the
    /// `api_key()` gate, which keys on `paired`, returns Some).
    fn paired_state() -> PairingState {
        use std::io::Write;
        let mut p = std::env::temp_dir();
        p.push(format!("ados-logpush-pair-{}.json", std::process::id()));
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(br#"{"paired":true,"api_key":"k-123","owner_id":"u1"}"#)
            .unwrap();
        let s = PairingState::load_from(&p);
        let _ = std::fs::remove_file(&p);
        s
    }

    // ── result file write ───────────────────────────────────────────────

    #[test]
    fn write_result_is_readable_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logd-push-result.json");
        let r = PushResult {
            pushed: 1,
            deduped: 0,
            bytes: 123,
            error: None,
        };
        write_result(&path, &r);
        let text = std::fs::read_to_string(&path).unwrap();
        let back: PushResult = serde_json::from_str(&text).unwrap();
        assert_eq!(r, back);
        // No leftover temp file.
        assert!(!path.with_extension("json.tmp").exists());
    }

    // ── http response parsing (chunked + content-length) ────────────────

    #[test]
    fn parse_http_response_reads_content_length_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        let (status, body) = parse_http_response(raw).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"hello");
    }

    #[test]
    fn parse_http_response_de_chunks_a_chunked_body() {
        // Two chunks ("ab" then "cde") then the terminator, binary-safe.
        let raw =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nab\r\n3\r\ncde\r\n0\r\n\r\n";
        let (status, body) = parse_http_response(raw).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"abcde");
    }

    #[test]
    fn parse_http_response_surfaces_non_2xx_status() {
        let raw = b"HTTP/1.1 403 Forbidden\r\nContent-Length: 2\r\n\r\nno";
        let (status, body) = parse_http_response(raw).unwrap();
        assert_eq!(status, 403);
        assert_eq!(body, b"no");
    }
}
