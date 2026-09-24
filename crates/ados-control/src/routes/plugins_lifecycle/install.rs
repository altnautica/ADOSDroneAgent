//! Parse and install: the multipart upload, the allowlisted URL, and the
//! built-in plugins the agent ships.
//!
//! The two-stage install dialog parses first (signature shape, manifest, the
//! permission preview; nothing touches disk), then installs after the operator
//! consents, granting the permissions it approved in the same call.

use std::path::Path;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::multipart::{Multipart, MultipartError, MultipartRejection};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use ados_plugin_host::archive::{parse_archive_bytes, ArchiveContents, ARCHIVE_MAX_BYTES};
use ados_plugin_host::download::{
    fetch_capped, verify_sha256, DownloadError, DownloadSource, HttpDownloadSource,
    DOWNLOAD_MAX_BYTES,
};
use ados_plugin_host::manifest::PluginManifest;
use ados_plugin_host::supervisor::{InstallResult, PluginSupervisor};

use super::jobs::JobSidecar;
use super::read::{bundled_catalog, halves, top_level_str};
use super::{json_response, respond, Refusal};
use crate::state::AppState;

/// The multipart body cap for an upload route: the archive cap plus room for
/// the multipart framing around it.
pub(crate) const UPLOAD_BODY_LIMIT: usize = ARCHIVE_MAX_BYTES as usize + 1024 * 1024;

/// The built-in plugins this agent ships, by id: their manifests, installed
/// unsigned from the agent's own package (the Python runner imports their code
/// from the agent venv). The same files the Python side loads.
pub(crate) static BUILTIN_MANIFESTS: [(&str, &str); 3] = [
    (
        "io.altnautica.geofence",
        include_str!("../../../../../src/ados/plugins/builtin/geofence/manifest.yaml"),
    ),
    (
        "io.altnautica.mavlink-inspector",
        include_str!("../../../../../src/ados/plugins/builtin/mavlink_inspector/manifest.yaml"),
    ),
    (
        "io.altnautica.telemetry-logger",
        include_str!("../../../../../src/ados/plugins/builtin/telemetry_logger/manifest.yaml"),
    ),
];

/// One install-dialog permission row: the id, whether it is required, and the
/// catalog copy when the capability is known. Field order is the Python
/// route's.
#[derive(Debug, Serialize)]
pub(crate) struct PermissionDto {
    id: String,
    required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    risk: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    risk_reason: Option<String>,
}

/// The owner and declaration of a plugin-declared capability: this manifest's
/// own, else an installed plugin's.
fn declared_by(
    cap_id: &str,
    manifest: Option<&PluginManifest>,
    sup: &PluginSupervisor,
) -> Option<(String, ados_plugin_host::manifest::DeclaredCapability)> {
    manifest
        .and_then(|m| {
            m.agent.as_ref().and_then(|a| {
                a.declared_capabilities
                    .iter()
                    .find(|c| c.id == cap_id)
                    .map(|c| (m.id.clone(), c.clone()))
            })
        })
        .or_else(|| sup.declared_capability(cap_id))
}

/// Build the install-dialog permission rows for `ids`. A catalog capability
/// carries its catalog copy; a plugin-declared one carries its declared
/// description and risk; any other id is the bare `{id, required}`. Every row
/// is `required` (the manifest schema does not distinguish optional ones).
pub(crate) fn enrich_permissions(
    ids: &[String],
    manifest: Option<&PluginManifest>,
    sup: &PluginSupervisor,
) -> Vec<PermissionDto> {
    ids.iter()
        .map(|id| {
            let mut row = PermissionDto {
                id: id.clone(),
                required: true,
                label: None,
                description: None,
                category: None,
                risk: None,
                risk_reason: None,
            };
            if let Some(meta) = ados_protocol::capabilities::get_agent_capability(id) {
                row.label = Some(meta.label.to_string());
                row.description = Some(meta.description.to_string());
                row.category = Some(meta.category.to_string());
                row.risk = Some(meta.risk.to_string());
                row.risk_reason = Some(meta.risk_reason.to_string());
            } else if let Some((owner, cap)) = declared_by(id, manifest, sup) {
                row.label = Some(id.clone());
                row.description = Some(cap.description);
                row.category = Some("plugin".to_string());
                row.risk = Some(cap.risk);
                row.risk_reason = Some(format!("Declared by the plugin {owner}."));
            }
            row
        })
        .collect()
}

/// Agent-half permission ids that name neither a catalog capability nor a
/// plugin-declared one, so the install dialog would have no label for them.
fn unknown_capabilities(manifest: &PluginManifest, sup: &PluginSupervisor) -> Vec<String> {
    let Some(agent) = &manifest.agent else {
        return Vec::new();
    };
    let mut ids: Vec<&str> = agent.permissions.iter().map(|p| p.id.as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    ids.into_iter()
        .filter(|id| {
            !ados_protocol::capabilities::is_known_agent_capability(id)
                && declared_by(id, Some(manifest), sup).is_none()
        })
        .map(str::to_string)
        .collect()
}

/// Refuse an archive declaring an agent capability the dialog cannot label.
fn check_capabilities_known(
    manifest: &PluginManifest,
    sup: &PluginSupervisor,
) -> Result<(), Refusal> {
    let unknown = unknown_capabilities(manifest, sup);
    if unknown.is_empty() {
        Ok(())
    } else {
        Err(Refusal::new(
            12,
            "manifest_invalid",
            format!("Unknown capability: {}", unknown.join(", ")),
            StatusCode::BAD_REQUEST,
        ))
    }
}

/// The parse preview, in the Python route's field order.
#[derive(Serialize)]
struct ParseSummary {
    ok: bool,
    plugin_id: String,
    version: String,
    name: String,
    description: String,
    author: String,
    license: String,
    risk: String,
    signer_id: Option<String>,
    signed: bool,
    halves: Vec<&'static str>,
    permissions: Vec<PermissionDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    archive_sha256: Option<String>,
}

/// Parse archive bytes into the preview. Every structural, manifest and
/// unknown-capability fault is `12 manifest_invalid`; a malformed signature is
/// `10 signature_<kind>`, as the Python preview mapped them.
fn summarize(raw: Vec<u8>, sup: &PluginSupervisor) -> Result<ParseSummary, Refusal> {
    let contents = parse_archive_bytes(raw).map_err(|e| match Refusal::from_install(e) {
        r if r.code == 10 => r,
        r => Refusal::new(12, "manifest_invalid", r.detail, StatusCode::BAD_REQUEST),
    })?;
    let manifest = &contents.manifest;
    check_capabilities_known(manifest, sup)?;
    let permission_ids: Vec<String> = manifest.declared_permissions().into_iter().collect();
    Ok(ParseSummary {
        ok: true,
        plugin_id: manifest.id.clone(),
        version: manifest.version.clone(),
        name: manifest.name.clone(),
        description: manifest.description.clone(),
        author: top_level_str(manifest, "author"),
        license: top_level_str(manifest, "license"),
        risk: manifest.risk.clone(),
        signer_id: contents.signer_id.clone(),
        signed: contents.signature_b64.is_some(),
        halves: halves(manifest),
        permissions: enrich_permissions(&permission_ids, Some(manifest), sup),
        archive_sha256: None,
    })
}

/// A refused multipart read: an over-cap body is `13 archive_too_large`,
/// anything else a malformed upload.
fn multipart_refusal(e: MultipartError) -> Refusal {
    if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
        too_large(ARCHIVE_MAX_BYTES)
    } else {
        Refusal::usage("usage_error", format!("malformed multipart upload: {e}"))
    }
}

fn too_large(cap: u64) -> Refusal {
    Refusal::new(
        13,
        "archive_too_large",
        format!("archive exceeds {cap} byte cap"),
        StatusCode::PAYLOAD_TOO_LARGE,
    )
}

/// Read the `file` part of an upload: it must be named `*.adosplug`, be
/// non-empty and fit the archive cap. Returns `(file name, bytes)`.
async fn read_upload(
    multipart: Result<Multipart, MultipartRejection>,
) -> Result<(String, Vec<u8>), Refusal> {
    let expected = || Refusal::usage("usage_error", "expected a .adosplug file");
    let mut multipart = multipart.map_err(|_| expected())?;
    while let Some(mut field) = multipart.next_field().await.map_err(multipart_refusal)? {
        if field.name() != Some("file") {
            continue;
        }
        let file_name = field.file_name().unwrap_or_default().to_string();
        if !file_name.ends_with(".adosplug") {
            return Err(expected());
        }
        let mut raw: Vec<u8> = Vec::new();
        while let Some(chunk) = field.chunk().await.map_err(multipart_refusal)? {
            if (raw.len() + chunk.len()) as u64 > ARCHIVE_MAX_BYTES {
                return Err(too_large(ARCHIVE_MAX_BYTES));
            }
            raw.extend_from_slice(&chunk);
        }
        if raw.is_empty() {
            return Err(Refusal::usage("usage_error", "empty upload"));
        }
        return Ok((file_name, raw));
    }
    Err(expected())
}

/// `POST /api/plugins/parse`: multipart `file`, the non-committing preview.
pub async fn parse_plugin(
    State(state): State<AppState>,
    multipart: Result<Multipart, MultipartRejection>,
) -> Response {
    let result = async {
        let (_, raw) = read_upload(multipart).await?;
        state.plugins.read(move |sup| summarize(raw, sup)).await
    }
    .await;
    respond(result.map(|summary| json_response(StatusCode::OK, &summary)))
}

/// A refused download, in the Python route's taxonomy.
fn download_refusal(e: DownloadError) -> Refusal {
    match e {
        DownloadError::TooLarge(cap) => too_large(cap),
        DownloadError::Sha256Mismatch { .. } => Refusal::new(
            12,
            "sha256_mismatch",
            "archive sha256 did not match pin",
            StatusCode::BAD_REQUEST,
        ),
        DownloadError::Empty
        | DownloadError::Unparseable
        | DownloadError::NotHttps
        | DownloadError::NoHost
        | DownloadError::HostNotAllowed(_) => Refusal::usage("url_invalid", e.to_string()),
        DownloadError::Transport(_) | DownloadError::Io(_) => Refusal::new(
            20,
            "download_failed",
            e.to_string(),
            StatusCode::BAD_GATEWAY,
        ),
    }
}

/// Fetch an archive from an allowlisted URL under the archive download cap and
/// check it against `expected_sha256` (skipped when empty). Returns the bytes
/// and their sha256. Runs on a blocking thread: the live client is blocking.
async fn download_archive(
    source: Option<Arc<dyn DownloadSource>>,
    url: String,
    expected_sha256: String,
) -> Result<(Vec<u8>, String), Refusal> {
    tokio::task::spawn_blocking(move || {
        let live;
        let source: &dyn DownloadSource = match &source {
            Some(s) => s.as_ref(),
            None => {
                live = HttpDownloadSource::new();
                &live
            }
        };
        let body = fetch_capped(source, &url, DOWNLOAD_MAX_BYTES).map_err(download_refusal)?;
        verify_sha256(&body, &expected_sha256).map_err(|e| {
            tracing::debug!(error = %e, "plugin archive sha256 mismatch");
            download_refusal(e)
        })?;
        let sha256 = hex::encode(Sha256::digest(&body));
        Ok((body, sha256))
    })
    .await
    .map_err(|e| Refusal::host_io(format!("download task failed: {e}")))?
}

/// A request body that did not parse: answered with FastAPI's `422`
/// `{"detail": ...}` shape.
pub(crate) struct BadBody(String);

impl IntoResponse for BadBody {
    fn into_response(self) -> Response {
        crate::routes::detail(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("invalid request body: {}", self.0),
        )
    }
}

/// Parse a JSON request body.
pub(crate) fn parse_body<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, BadBody> {
    serde_json::from_slice(body).map_err(|e| BadBody(e.to_string()))
}

#[derive(Deserialize)]
struct ParseFromUrlRequest {
    url: String,
    #[serde(default)]
    expected_sha256: Option<String>,
}

/// Validate an operator-supplied archive URL against the allowlist.
fn validate_url(raw: &str) -> Result<String, Refusal> {
    let url = raw.trim().to_string();
    if url.is_empty() {
        return Err(Refusal::usage("usage_error", "url required"));
    }
    ados_plugin_host::download::validate_download_url(&url)
        .map_err(|e| Refusal::usage("url_invalid", e.to_string()))?;
    Ok(url)
}

/// `POST /api/plugins/parse_from_url`: `{url, expected_sha256?}`. Downloads an
/// allowlisted archive and answers the preview plus the archive's sha256, so
/// the install that follows can pin the exact bytes the operator reviewed.
/// Nothing is written to the plugin store.
pub async fn parse_from_url(State(state): State<AppState>, body: Bytes) -> Response {
    let req: ParseFromUrlRequest = match parse_body(&body) {
        Ok(r) => r,
        Err(bad) => return bad.into_response(),
    };
    let result = async {
        let url = validate_url(&req.url)?;
        let expected = req.expected_sha256.unwrap_or_default().trim().to_string();
        let (raw, sha256) = download_archive(state.plugins.download.clone(), url, expected).await?;
        let mut summary = state.plugins.read(move |sup| summarize(raw, sup)).await?;
        summary.archive_sha256 = Some(sha256);
        Ok(summary)
    }
    .await;
    respond(result.map(|summary| json_response(StatusCode::OK, &summary)))
}

/// The install response, in the Python route's field order.
#[derive(Serialize)]
struct InstallResponse {
    ok: bool,
    plugin_id: String,
    version: String,
    signer_id: Option<String>,
    risk: String,
    permissions_requested: Vec<String>,
    granted: Vec<String>,
    job_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sha256: Option<String>,
}

impl InstallResponse {
    fn new(result: InstallResult, granted: Vec<String>, job_id: Option<String>) -> Self {
        Self {
            ok: true,
            plugin_id: result.plugin_id,
            version: result.version,
            signer_id: result.signer_id,
            risk: result.risk,
            permissions_requested: result.permissions_requested,
            granted,
            job_id,
            sha256: None,
        }
    }
}

/// Grant each requested permission; a refused one is logged and skipped.
fn grant_requested(sup: &mut PluginSupervisor, plugin_id: &str, wanted: &[String]) -> Vec<String> {
    let mut granted = Vec::new();
    for perm in wanted {
        match sup.grant_permission(plugin_id, perm) {
            Ok(()) => granted.push(perm.clone()),
            Err(e) => tracing::warn!(
                plugin_id,
                permission = %perm,
                error = %e,
                "plugin_install_grant_skip"
            ),
        }
    }
    granted
}

/// The shared install tail: preview checks, install, grants, with the job
/// sidecar walked through `verifying → installing → completed` (or `failed`
/// with the refusal) so the progress stream serves every transport alike.
async fn install_bytes(
    state: &AppState,
    raw: Vec<u8>,
    source_uri: String,
    job: &JobSidecar,
    wanted: Vec<String>,
) -> Result<(InstallResult, Vec<String>), Refusal> {
    job.stage("verifying");
    let contents: ArchiveContents = state
        .plugins
        .read(move |sup| {
            let contents = parse_archive_bytes(raw).map_err(Refusal::from_install)?;
            check_capabilities_known(&contents.manifest, sup)?;
            Ok(contents)
        })
        .await
        .map_err(|r| job.fail(r))?;
    job.stage("installing");
    let (result, granted) = state
        .plugins
        .write(move |sup| {
            let result = sup
                .install_contents(contents, Path::new(&source_uri))
                .map_err(Refusal::from_install)?;
            let granted = grant_requested(sup, &result.plugin_id, &wanted);
            Ok((result, granted))
        })
        .await
        .and_then(|r| r)
        .map_err(|r| job.fail(r))?;
    job.completed(&result.plugin_id);
    Ok((result, granted))
}

#[derive(Deserialize)]
pub struct InstallQuery {
    #[serde(default)]
    job_id: Option<String>,
    #[serde(default)]
    requested_permissions: Option<String>,
}

/// `POST /api/plugins/install?job_id=&requested_permissions=a,b`: multipart
/// `file`. Installs the archive and grants each requested permission the
/// supervisor accepts.
pub async fn install_plugin(
    State(state): State<AppState>,
    Query(query): Query<InstallQuery>,
    multipart: Result<Multipart, MultipartRejection>,
) -> Response {
    let result = async {
        let (file_name, raw) = read_upload(multipart).await?;
        let job = JobSidecar::new(&state.plugins.job_dir, query.job_id.clone());
        let wanted: Vec<String> = query
            .requested_permissions
            .as_deref()
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect();
        let (result, granted) = install_bytes(&state, raw, file_name, &job, wanted).await?;
        Ok(InstallResponse::new(result, granted, query.job_id))
    }
    .await;
    respond(result.map(|body| json_response(StatusCode::OK, &body)))
}

#[derive(Deserialize)]
struct InstallFromUrlRequest {
    url: String,
    #[serde(default)]
    expected_sha256: Option<String>,
    #[serde(default)]
    requested_permissions: Option<Vec<String>>,
    #[serde(default)]
    job_id: Option<String>,
    #[serde(default)]
    from_catalog: bool,
}

/// The archive sha256 the bundled catalog pins for `url`, when `url` is one of
/// its entries' download URLs.
fn catalog_pin_for(url: &str) -> Option<String> {
    bundled_catalog()
        .get("plugins")?
        .as_array()?
        .iter()
        .find(|p| {
            p.get("download_url")
                .and_then(|u| u.as_str())
                .is_some_and(|u| !u.is_empty() && u == url)
        })
        .map(|p| {
            p.get("archive_sha256")
                .and_then(|s| s.as_str())
                .unwrap_or_default()
                .to_string()
        })
}

/// `POST /api/plugins/install_from_url`: `{url, expected_sha256?,
/// requested_permissions?, job_id?, from_catalog}`. Downloads an allowlisted
/// archive (streamed under the cap, pinned when a sha256 is given) and installs
/// it. A catalog install must pin a sha256, and one naming a URL the bundled
/// catalog lists must pin that entry's sha256.
pub async fn install_from_url(State(state): State<AppState>, body: Bytes) -> Response {
    let req: InstallFromUrlRequest = match parse_body(&body) {
        Ok(r) => r,
        Err(bad) => return bad.into_response(),
    };
    let result = async {
        let url = validate_url(&req.url)?;
        let expected = req.expected_sha256.unwrap_or_default().trim().to_string();
        if req.from_catalog {
            if expected.is_empty() {
                return Err(Refusal::usage(
                    "sha256_required",
                    "catalog installs must pin archive_sha256",
                ));
            }
            if let Some(pin) = catalog_pin_for(&url) {
                if !pin.eq_ignore_ascii_case(&expected) {
                    return Err(Refusal::new(
                        12,
                        "catalog_mismatch",
                        "archive_sha256 does not match the bundled catalog entry for this url",
                        StatusCode::BAD_REQUEST,
                    ));
                }
            }
        }
        let job = JobSidecar::new(&state.plugins.job_dir, req.job_id.clone());
        job.stage("downloading");
        let (raw, sha256) = download_archive(state.plugins.download.clone(), url.clone(), expected)
            .await
            .map_err(|r| job.fail(r))?;
        let wanted: Vec<String> = req
            .requested_permissions
            .unwrap_or_default()
            .iter()
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect();
        let bytes = raw.len();
        let (result, granted) = install_bytes(&state, raw, url, &job, wanted).await?;
        tracing::info!(
            plugin_id = %result.plugin_id,
            version = %result.version,
            sha256 = %sha256,
            bytes,
            "plugin_install_from_url_ok"
        );
        let mut body = InstallResponse::new(result, granted, req.job_id);
        body.sha256 = Some(sha256);
        Ok(body)
    }
    .await;
    respond(result.map(|body| json_response(StatusCode::OK, &body)))
}

#[derive(Deserialize)]
struct InstallBuiltinRequest {
    plugin_id: String,
}

/// `POST /api/plugins/install_builtin`: `{plugin_id}`. Installs one of the
/// built-in plugins the agent ships. They carry no archive signature (their
/// code is the agent's own); every other install gate applies.
pub async fn install_builtin(State(state): State<AppState>, body: Bytes) -> Response {
    let req: InstallBuiltinRequest = match parse_body(&body) {
        Ok(r) => r,
        Err(bad) => return bad.into_response(),
    };
    let Some(&(_, yaml)) = BUILTIN_MANIFESTS
        .iter()
        .find(|(id, _)| *id == req.plugin_id)
    else {
        return respond(Err(Refusal::not_found(format!(
            "{} is not a built-in plugin",
            req.plugin_id
        ))));
    };
    let result = state
        .plugins
        .write(move |sup| sup.install_builtin(yaml).map_err(Refusal::from_install))
        .await
        .and_then(|r| r);
    respond(result.map(|installed| {
        json_response(
            StatusCode::OK,
            &InstallResponse::new(installed, Vec::new(), None),
        )
    }))
}
