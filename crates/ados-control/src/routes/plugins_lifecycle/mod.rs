//! Native plugin lifecycle: `/api/plugins/*` and `/api/v1/plugins/catalog`.
//!
//! The operator surface Mission Control's `PluginAgentClient`, the on-box
//! dashboard and the `ados plugin` CLI call: list, detail, the GCS asset,
//! manifest and attestation reads, parse, install (upload, URL, built-in),
//! grant, revoke, enable, disable, remove, update preferences, the capability
//! token mint, the bundled catalog and the install-job progress stream. Every
//! mutation drives the Rust [`PluginSupervisor`], the same controller the cloud
//! relay and the plugin-host daemon use; the plugin state file and its flock
//! are the cross-process consistency point.
//!
//! Errors keep the envelope the Python routes answered with,
//! `{"ok": false, "code": N, "kind": "...", "detail": "..."}`, with the same
//! codes and statuses:
//!
//! | code | kind | status |
//! |---|---|---|
//! | 2 | `usage_error` / `url_invalid` / `sha256_required` | 400 |
//! | 10 | `signature_<missing\|invalid\|revoked\|unknown_signer>` | 400 |
//! | 11 | `permission_deny` (400) / `not_paired` (409) | |
//! | 12 | `manifest_invalid` / `archive_invalid` / `sha256_mismatch` / `catalog_mismatch` | 400 |
//! | 13 | `archive_too_large` | 413 |
//! | 14 | `not_found` | 404 |
//! | 17 | `ados_version_skew` | 409 |
//! | 18 | `incompatible` (target profile or host binary) | 409 |
//! | 19 | `refused` (a first-party-only gate) | 403 |
//! | 20 | `host_io_error` (500) / `download_failed` (502) | |
//!
//! Codes 18 and 19 are new: the Python supervisor had neither the profile gate
//! nor the first-party-only gates, and folded every other supervisor refusal
//! into `20 host_io_error`, which these routes still do for the refusals it
//! shared (board, tier, downgrade).
//!
//! Lifecycle writes are serialized on one [`tokio::sync::Mutex`] and run on a
//! blocking thread (archive unpack, signature check, service backend calls,
//! payload downloads). Each write runs on a supervisor rebuilt from the node's
//! current facts (agent version, board, profile) at the start of the write:
//! every piece of supervisor state lives on disk and is re-read under the state
//! lock anyway, so a rebuild loses nothing and an agent upgrade or a late board
//! detection is honoured without restarting this process. Reads build their own
//! short-lived supervisor and never wait on the write mutex, so a long payload
//! download does not stall the plugin list.

mod install;
mod jobs;
mod manage;
mod read;
mod token;

#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use ados_plugin_host::download::DownloadSource;
use ados_plugin_host::errors::LifecycleError;
use ados_plugin_host::supervisor::{Paths, PluginSupervisor};

pub(crate) use install::UPLOAD_BODY_LIMIT;
pub use install::{
    install_builtin, install_from_url, install_plugin, parse_from_url, parse_plugin,
};
pub use jobs::stream_install_job;
pub use manage::{
    disable_plugin, enable_plugin, grant_permission, pin_plugin, remove_plugin, revoke_permission,
    set_auto_update, unpin_plugin,
};
pub use read::{
    get_attestation, get_catalog, get_gcs_asset, get_manifest, get_plugin, get_readiness,
    list_plugins,
};
pub use token::mint_capability_token;

/// Builds a supervisor configured for this node.
type SupervisorFactory = dyn Fn() -> PluginSupervisor + Send + Sync;

/// The lifecycle state the routes share: the write lock around the supervisor,
/// the factory that configures one, and the seams tests redirect.
#[derive(Clone)]
pub struct PluginLifecycle {
    /// The write lock. Held for the whole of a lifecycle write, so two writes
    /// never interleave on the state file within this process.
    supervisor: Arc<tokio::sync::Mutex<PluginSupervisor>>,
    factory: Arc<SupervisorFactory>,
    /// The archive transport for URL installs. `None` builds the live HTTPS
    /// client on the blocking thread that uses it.
    download: Option<Arc<dyn DownloadSource>>,
    /// Where install-job progress sidecars are written and polled.
    job_dir: PathBuf,
    /// The agent run dir holding each plugin's `plugin-http/<id>/http.sock`.
    run_dir: PathBuf,
    /// The residual API socket enable asks for model delivery. `None` resolves
    /// the live one (`ADOS_API_INTERNAL_SOCKET`) per call.
    residual_socket: Option<PathBuf>,
}

impl PluginLifecycle {
    /// The live configuration: the plugin paths from the environment, signature
    /// enforcement on (unless `ADOS_PLUGIN_REQUIRE_SIGNED` relaxes it), and the
    /// agent version, board identity and node profile read fresh each time a
    /// supervisor is built.
    pub fn production(board_path: PathBuf) -> Self {
        let paths = Paths::from_env();
        let run_dir = paths.run_dir.clone();
        Self::new(
            move || {
                let (board_id, tier) = PluginSupervisor::board_identity(&board_path);
                PluginSupervisor::production(
                    Paths::from_env(),
                    board_id,
                    crate::state::agent_version(),
                )
                .with_profile(ados_config::node_profile())
                .with_board_tier(tier)
                .with_ungrantable_caps(ados_plugin_host::realhost::RealHost::ungrantable_caps())
            },
            None,
            run_dir.clone(),
            run_dir,
        )
    }

    /// A lifecycle over an explicit supervisor factory, archive transport, job
    /// sidecar dir and run dir.
    pub fn new(
        factory: impl Fn() -> PluginSupervisor + Send + Sync + 'static,
        download: Option<Arc<dyn DownloadSource>>,
        job_dir: PathBuf,
        run_dir: PathBuf,
    ) -> Self {
        let factory: Arc<SupervisorFactory> = Arc::new(factory);
        Self {
            supervisor: Arc::new(tokio::sync::Mutex::new(factory())),
            factory,
            download,
            job_dir,
            run_dir,
            residual_socket: None,
        }
    }

    /// Point enable's model delivery at an explicit residual API socket.
    pub fn with_model_delivery_socket(mut self, socket: PathBuf) -> Self {
        self.residual_socket = Some(socket);
        self
    }

    /// The residual API socket model delivery is requested on.
    fn model_delivery_socket(&self) -> PathBuf {
        self.residual_socket
            .clone()
            .unwrap_or_else(crate::proxy::default_internal_socket)
    }

    /// Run one lifecycle write on a blocking thread, holding the write lock and
    /// on a supervisor rebuilt from the node's current facts.
    async fn write<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut PluginSupervisor) -> T + Send + 'static,
    ) -> Result<T, Refusal> {
        let lock = Arc::clone(&self.supervisor);
        let factory = Arc::clone(&self.factory);
        tokio::task::spawn_blocking(move || {
            let mut sup = lock.blocking_lock();
            *sup = factory();
            f(&mut sup)
        })
        .await
        .map_err(|e| Refusal::host_io(format!("plugin lifecycle task failed: {e}")))
    }

    /// Run one read on a blocking thread against a freshly loaded supervisor.
    /// A state file that cannot be read is a `20 host_io_error`.
    async fn read<T: Send + 'static>(
        &self,
        f: impl FnOnce(&PluginSupervisor) -> Result<T, Refusal> + Send + 'static,
    ) -> Result<T, Refusal> {
        let factory = Arc::clone(&self.factory);
        tokio::task::spawn_blocking(move || {
            let mut sup = factory();
            sup.refresh().map_err(|e| Refusal::host_io(e.to_string()))?;
            f(&sup)
        })
        .await
        .map_err(|e| Refusal::host_io(format!("plugin lifecycle task failed: {e}")))?
    }

    /// The HTTP socket a plugin serves its own API on.
    pub(crate) fn http_socket(&self, plugin_id: &str) -> PathBuf {
        ados_plugin_host::systemd::plugin_http_dir(&self.run_dir, plugin_id)
            .join(ados_plugin_host::systemd::PLUGIN_HTTP_SOCKET_NAME)
    }
}

/// One refused request in the lifecycle envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refusal {
    pub code: u16,
    pub kind: String,
    pub detail: String,
    pub status: StatusCode,
}

/// The wire form. Field order is the Python dict's, so the body is byte-equal.
#[derive(Serialize)]
struct Envelope<'a> {
    ok: bool,
    code: u16,
    kind: &'a str,
    detail: &'a str,
}

impl Refusal {
    pub(crate) fn new(
        code: u16,
        kind: impl Into<String>,
        detail: impl Into<String>,
        status: StatusCode,
    ) -> Self {
        Self {
            code,
            kind: kind.into(),
            detail: detail.into(),
            status,
        }
    }

    pub(crate) fn usage(kind: &str, detail: impl Into<String>) -> Self {
        Self::new(2, kind, detail, StatusCode::BAD_REQUEST)
    }

    pub(crate) fn not_found(detail: impl Into<String>) -> Self {
        Self::new(14, "not_found", detail, StatusCode::NOT_FOUND)
    }

    pub(crate) fn not_installed(plugin_id: &str) -> Self {
        Self::not_found(format!("plugin {plugin_id} not installed"))
    }

    pub(crate) fn host_io(detail: impl Into<String>) -> Self {
        Self::new(
            20,
            "host_io_error",
            detail,
            StatusCode::INTERNAL_SERVER_ERROR,
        )
    }

    /// A supervisor refusal on a lifecycle operation (enable, disable, grant,
    /// remove...): a missing plugin is `14 not_found`, anything else
    /// `20 host_io_error`, as the Python routes mapped them.
    pub(crate) fn from_operation(err: &LifecycleError) -> Self {
        let msg = err.to_string();
        if msg.contains("not installed") {
            Self::not_found(msg)
        } else {
            Self::host_io(msg)
        }
    }

    /// An install refusal: signature, manifest and archive faults by type,
    /// supervisor refusals by their stable message prefix.
    pub(crate) fn from_install(err: LifecycleError) -> Self {
        match err {
            LifecycleError::Signature(e) => Self::new(
                10,
                format!("signature_{}", e.kind.as_str()),
                e.message,
                StatusCode::BAD_REQUEST,
            ),
            LifecycleError::Manifest(e) => {
                Self::new(12, "manifest_invalid", e.0, StatusCode::BAD_REQUEST)
            }
            LifecycleError::Archive(e) => {
                Self::new(12, "archive_invalid", e.0, StatusCode::BAD_REQUEST)
            }
            LifecycleError::Supervisor(e) => Self::from_supervisor_refusal(e.0),
            LifecycleError::Io(e) => Self::host_io(format!("state io error: {e}")),
        }
    }

    fn from_supervisor_refusal(msg: String) -> Self {
        if msg.contains("ADOS version") {
            Self::new(17, "ados_version_skew", msg, StatusCode::CONFLICT)
        } else if msg.starts_with("incompatible:") {
            Self::new(18, "incompatible", msg, StatusCode::CONFLICT)
        } else if msg.starts_with("refused:") {
            Self::new(19, "refused", msg, StatusCode::FORBIDDEN)
        } else {
            Self::host_io(msg)
        }
    }
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        json_response(
            self.status,
            &Envelope {
                ok: false,
                code: self.code,
                kind: &self.kind,
                detail: &self.detail,
            },
        )
    }
}

/// A JSON response serialized in the value's own field order (a struct keeps
/// its declaration order, unlike `serde_json::Value`, whose maps sort).
pub(crate) fn json_response<T: Serialize>(status: StatusCode, body: &T) -> Response {
    match serde_json::to_vec(body) {
        Ok(bytes) => (
            status,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            bytes,
        )
            .into_response(),
        Err(e) => crate::routes::detail(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("response encode failed: {e}"),
        ),
    }
}

/// Collapse a handler result into a response.
pub(crate) fn respond(result: Result<Response, Refusal>) -> Response {
    result.unwrap_or_else(IntoResponse::into_response)
}

/// Whether `id` can name a plugin in a filesystem path: lowercase alphanumeric
/// first, then lowercase alphanumeric, `.` or `-`, no `..`, at most 128 bytes.
/// axum percent-decodes path segments, so this runs before any join.
pub(crate) fn is_plugin_id(id: &str) -> bool {
    id.len() <= 128 && crate::routes::plugins_state::is_state_id(id)
}

/// Parse a query flag the way FastAPI's `bool` parameter does.
pub(crate) fn query_bool(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "t" | "yes" | "y" | "on" => Some(true),
        "0" | "false" | "f" | "no" | "n" | "off" => Some(false),
        _ => None,
    }
}

/// Epoch milliseconds.
pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
