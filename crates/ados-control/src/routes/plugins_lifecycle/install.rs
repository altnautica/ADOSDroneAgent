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
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use ados_plugin_host::archive::{parse_archive_bytes, ArchiveContents, ARCHIVE_MAX_BYTES};
use ados_plugin_host::download::{
    fetch_capped, verify_sha256, DownloadError, DownloadSource, HttpDownloadSource,
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

#[path = "upload_admission.rs"]
mod admission;
use admission::{busy, expected_digest, try_archive_slot, UploadFault};

mod apply;

pub use apply::install_builtin;
pub use apply::install_from_url;
pub use apply::install_plugin;

/// Read the `file` part of an upload: it must be named `*.adosplug`, be
/// non-empty and fit the archive cap. The bytes are hashed as they stream in;
/// when `expected` is set the digest must match it. Returns `(file name,
/// bytes)`.
async fn read_upload(
    multipart: Result<Multipart, MultipartRejection>,
    expected: Option<&str>,
) -> Result<(String, Vec<u8>), UploadFault> {
    let expected_file = || Refusal::usage("usage_error", "expected a .adosplug file");
    let mut multipart = multipart.map_err(|_| expected_file())?;
    while let Some(mut field) = multipart.next_field().await.map_err(multipart_refusal)? {
        if field.name() != Some("file") {
            continue;
        }
        let file_name = field.file_name().unwrap_or_default().to_string();
        if !file_name.ends_with(".adosplug") {
            return Err(expected_file().into());
        }
        let mut raw: Vec<u8> = Vec::new();
        let mut hasher = Sha256::new();
        while let Some(chunk) = field.chunk().await.map_err(multipart_refusal)? {
            if (raw.len() + chunk.len()) as u64 > ARCHIVE_MAX_BYTES {
                return Err(too_large(ARCHIVE_MAX_BYTES).into());
            }
            hasher.update(&chunk);
            raw.extend_from_slice(&chunk);
        }
        if raw.is_empty() {
            return Err(Refusal::usage("usage_error", "empty upload").into());
        }
        if let Some(expected) = expected {
            if hex::encode(hasher.finalize()) != expected {
                return Err(UploadFault::DigestMismatch);
            }
        }
        return Ok((file_name, raw));
    }
    Err(expected_file().into())
}

/// `POST /api/plugins/parse`: multipart `file`, the non-committing preview.
pub async fn parse_plugin(
    State(state): State<AppState>,
    headers: HeaderMap,
    multipart: Result<Multipart, MultipartRejection>,
) -> Response {
    let Some(_slot) = try_archive_slot() else {
        return busy();
    };
    let expected = match expected_digest(&headers) {
        Ok(d) => d,
        Err(r) => return r.into_response(),
    };
    let raw = match read_upload(multipart, expected.as_deref()).await {
        Ok((_, raw)) => raw,
        Err(fault) => return fault.respond(None),
    };
    let result = state.plugins.read(move |sup| summarize(raw, sup)).await;
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
        DownloadError::Transport(_) | DownloadError::Io(_) | DownloadError::InvalidPin(_) => {
            Refusal::new(
                20,
                "download_failed",
                e.to_string(),
                StatusCode::BAD_GATEWAY,
            )
        }
    }
}

/// Fetch an archive from an allowlisted URL under the archive cap and check it
/// against `expected_sha256` (skipped when empty). Returns the bytes and their
/// sha256. The caller holds the archive slot; the fetch runs on a blocking
/// thread because the live client is blocking.
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
        let body = fetch_capped(source, &url, ARCHIVE_MAX_BYTES).map_err(download_refusal)?;
        let pin = (!expected_sha256.is_empty()).then_some(expected_sha256.as_str());
        verify_sha256(&body, pin).map_err(|e| {
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
    let Some(_slot) = try_archive_slot() else {
        return busy();
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

#[cfg(test)]
#[path = "install_tests.rs"]
mod tests;
