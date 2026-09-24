//! Install-job progress: the sidecar the install routes write and the
//! WebSocket that streams it.
//!
//! An install that carries a `job_id` records its stage in
//! `<run dir>/plugin_install_<job>.json` (`downloading`, `verifying`,
//! `installing`, then `completed` with `pluginId` or `failed` with `kind` and
//! `detail`). `WS /api/plugins/jobs/{job_id}` polls that file and sends each new
//! version, closing on a terminal stage or after ten idle minutes. The
//! transport is the file, so the stream serves any writer alike.
//!
//! A browser authenticates the handshake with a ticket scoped
//! `plugins.install_job:<job_id>` (checked at the LAN edge against this path);
//! the route echoes the `ados-ws-ticket` subprotocol it offered.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as AxumPath, State};
use axum::response::Response;
use serde_json::{json, Map, Value};

use super::{now_ms, Refusal};
use crate::state::AppState;

/// The subprotocol marker a browser offers its ticket under.
const WS_TICKET_SUBPROTOCOL: &str = "ados-ws-ticket";

/// How long the stream waits for a new stage before giving up.
const IDLE_TIMEOUT: Duration = Duration::from_secs(600);

/// How often the stream re-reads the sidecar.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Stages after which the job writes nothing more.
const TERMINAL_STAGES: [&str; 3] = ["completed", "failed", "cancelled"];

/// The sidecar path for `job_id`: the id keeps only alphanumerics, `-`, `_`
/// and `.`, so it cannot leave the dir. `None` when nothing survives.
pub(crate) fn sidecar_path(dir: &Path, job_id: &str) -> Option<PathBuf> {
    let safe: String = job_id
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect();
    (!safe.is_empty()).then(|| dir.join(format!("plugin_install_{safe}.json")))
}

/// Write one stage record atomically (a sibling temp file, then a rename), with
/// `jobId` (unless the fields carry one) and `updatedAt` stamped.
fn write_sidecar(dir: &Path, job_id: &str, fields: Map<String, Value>) -> std::io::Result<()> {
    let Some(path) = sidecar_path(dir, job_id) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "job_id is empty after sanitisation",
        ));
    };
    std::fs::create_dir_all(dir)?;
    let mut record = fields;
    record
        .entry("jobId")
        .or_insert_with(|| Value::String(job_id.to_string()));
    record.insert("updatedAt".to_string(), json!(now_ms()));
    let body = serde_json::to_vec(&Value::Object(record)).map_err(std::io::Error::other)?;
    let tmp = path.with_extension(format!("json.tmp-{}-{}", std::process::id(), now_ms()));
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// The progress record of one install request. A request without a `job_id`
/// records nothing.
pub(crate) struct JobSidecar {
    dir: PathBuf,
    job_id: Option<String>,
}

impl JobSidecar {
    pub(crate) fn new(dir: &Path, job_id: Option<String>) -> Self {
        Self {
            dir: dir.to_path_buf(),
            job_id: job_id.filter(|j| !j.is_empty()),
        }
    }

    fn write(&self, fields: Value) {
        let (Some(job_id), Value::Object(fields)) = (&self.job_id, fields) else {
            return;
        };
        if let Err(e) = write_sidecar(&self.dir, job_id, fields) {
            tracing::warn!(job_id = %job_id, error = %e, "plugin_install_sidecar_write_failed");
        }
    }

    /// Record an in-flight stage.
    pub(crate) fn stage(&self, stage: &str) {
        self.write(json!({ "stage": stage }));
    }

    /// Record the refusal as the job's failure and hand it back, so the stream
    /// shows the real reason instead of waiting out its idle timeout.
    pub(crate) fn fail(&self, refusal: Refusal) -> Refusal {
        self.write(json!({
            "stage": "failed",
            "detail": refusal.detail,
            "kind": refusal.kind,
        }));
        refusal
    }

    /// Record the installed plugin.
    pub(crate) fn completed(&self, plugin_id: &str) {
        self.write(json!({ "stage": "completed", "pluginId": plugin_id }));
    }
}

/// Read the sidecar's content and mtime from one open file, so the two always
/// describe the same version (an atomic replace lands wholly before or after
/// the open). `None` when absent, unreadable or not JSON.
async fn read_snapshot(path: &Path) -> Option<(Value, SystemTime)> {
    use tokio::io::AsyncReadExt;
    let mut file = tokio::fs::File::open(path).await.ok()?;
    let mtime = file.metadata().await.ok()?.modified().ok()?;
    let mut body = Vec::new();
    file.read_to_end(&mut body).await.ok()?;
    let payload = serde_json::from_slice(&body).ok()?;
    Some((payload, mtime))
}

/// `WS /api/plugins/jobs/{job_id}`: stream the job's stage records.
pub async fn stream_install_job(
    State(state): State<AppState>,
    AxumPath(job_id): AxumPath<String>,
    ws: WebSocketUpgrade,
) -> Response {
    let dir = state.plugins.job_dir.clone();
    ws.protocols([WS_TICKET_SUBPROTOCOL])
        .on_upgrade(move |socket| run_job_stream(socket, dir, job_id, IDLE_TIMEOUT, POLL_INTERVAL))
}

/// Poll the sidecar and send each new version until a terminal stage, the
/// idle timeout (answered `{stage: cancelled, reason: idle_timeout}`), or the
/// client going away.
pub(crate) async fn run_job_stream(
    mut socket: WebSocket,
    dir: PathBuf,
    job_id: String,
    idle_timeout: Duration,
    poll: Duration,
) {
    let Some(path) = sidecar_path(&dir, &job_id) else {
        close(socket).await;
        return;
    };
    let mut last_mtime: Option<SystemTime> = None;
    let mut idle_since = Instant::now();
    loop {
        if let Some((payload, mtime)) = read_snapshot(&path).await {
            if last_mtime != Some(mtime) {
                last_mtime = Some(mtime);
                idle_since = Instant::now();
                let terminal = payload
                    .get("stage")
                    .and_then(Value::as_str)
                    .is_some_and(|s| TERMINAL_STAGES.contains(&s));
                if socket
                    .send(Message::Text(payload.to_string()))
                    .await
                    .is_err()
                {
                    return;
                }
                if terminal {
                    close(socket).await;
                    return;
                }
            }
        }
        if idle_since.elapsed() > idle_timeout {
            let cancelled =
                json!({"stage": "cancelled", "jobId": job_id, "reason": "idle_timeout"});
            if socket
                .send(Message::Text(cancelled.to_string()))
                .await
                .is_ok()
            {
                close(socket).await;
            }
            return;
        }
        tokio::select! {
            () = tokio::time::sleep(poll) => {}
            incoming = socket.recv() => match incoming {
                // The client's frames carry nothing for this stream.
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return,
                Some(Ok(_)) => {}
            },
        }
    }
}

/// Close normally.
async fn close(mut socket: WebSocket) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code: axum::extract::ws::close_code::NORMAL,
            reason: "".into(),
        })))
        .await;
}
