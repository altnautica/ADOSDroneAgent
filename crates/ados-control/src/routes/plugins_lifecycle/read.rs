//! The lifecycle reads: the install list and detail, a GCS asset, the installed
//! manifest, the install attestation, service readiness and the bundled catalog.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use axum::extract::{Path as AxumPath, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::Value;

use ados_plugin_host::attestation::Attestation;
use ados_plugin_host::manifest::{gcs_block_json, PluginManifest};
use ados_plugin_host::state::PluginInstall;
use ados_plugin_host::supervisor::PluginSupervisor;

use super::install::{enrich_permissions, PermissionDto};
use super::{is_plugin_id, json_response, respond, Refusal};
use crate::state::AppState;

/// The first-party catalog shipped with this build.
const BUNDLED_CATALOG: &str = include_str!("../../../../../src/ados/data/plugin-catalog.json");

/// The bundled catalog, parsed. A catalog that does not parse (a broken build)
/// reads as the empty catalog with the parse error, the shape the Python route
/// answered with when its bundled file was unreadable.
pub(crate) fn bundled_catalog() -> Value {
    serde_json::from_str(BUNDLED_CATALOG).unwrap_or_else(|e| {
        serde_json::json!({
            "schema_version": 1,
            "source": "first-party-bundled",
            "plugins": [],
            "error": e.to_string(),
        })
    })
}

/// `GET /api/v1/plugins/catalog`: the first-party catalog bundled with the
/// agent, so a fully local install still has a browse surface.
pub async fn get_catalog() -> Response {
    json_response(StatusCode::OK, &bundled_catalog())
}

/// One permission grant on the wire.
#[derive(Serialize)]
struct GrantDto {
    granted: bool,
    granted_at: Option<i64>,
}

/// One install record on the wire, in the Python route's field order.
#[derive(Serialize)]
pub(crate) struct InstallDto {
    plugin_id: String,
    version: String,
    source: Value,
    source_uri: Option<String>,
    signer_id: Option<String>,
    manifest_hash: String,
    status: Value,
    installed_at: i64,
    enabled_at: Option<i64>,
    permissions: BTreeMap<String, GrantDto>,
}

impl From<&PluginInstall> for InstallDto {
    fn from(install: &PluginInstall) -> Self {
        Self {
            plugin_id: install.plugin_id.clone(),
            version: install.version.clone(),
            source: serde_json::to_value(install.source).unwrap_or(Value::Null),
            source_uri: install.source_uri.clone(),
            signer_id: install.signer_id.clone(),
            manifest_hash: install.manifest_hash.clone(),
            status: serde_json::to_value(install.status).unwrap_or(Value::Null),
            installed_at: install.installed_at,
            enabled_at: install.enabled_at,
            permissions: install
                .permissions
                .iter()
                .map(|(id, g)| {
                    (
                        id.clone(),
                        GrantDto {
                            granted: g.granted,
                            granted_at: g.granted_at,
                        },
                    )
                })
                .collect(),
        }
    }
}

#[derive(Serialize)]
struct ListDto {
    installs: Vec<InstallDto>,
}

/// `GET /api/plugins` → `{installs: [...]}`.
pub async fn list_plugins(State(state): State<AppState>) -> Response {
    respond(
        state
            .plugins
            .read(|sup| {
                Ok(ListDto {
                    installs: sup.installs().iter().map(InstallDto::from).collect(),
                })
            })
            .await
            .map(|body| json_response(StatusCode::OK, &body)),
    )
}

#[derive(Serialize)]
struct McpDto {
    tools: Vec<Value>,
    resources: Vec<Value>,
    prompts: Vec<Value>,
}

#[derive(Serialize)]
struct ManifestDto {
    id: String,
    version: String,
    name: String,
    risk: String,
    license: String,
    halves: Vec<&'static str>,
    gcs: Option<Value>,
    permissions: Vec<PermissionDto>,
    mcp: McpDto,
}

#[derive(Serialize)]
struct DetailDto {
    install: InstallDto,
    granted_capabilities: Vec<String>,
    manifest: ManifestDto,
}

/// The halves a manifest carries, agent first.
pub(crate) fn halves(manifest: &PluginManifest) -> Vec<&'static str> {
    let mut out = Vec::with_capacity(2);
    if manifest.agent.is_some() {
        out.push("agent");
    }
    if manifest.gcs.is_some() {
        out.push("gcs");
    }
    out
}

/// A free-form top-level string field (`license`, `author`), `""` when absent.
pub(crate) fn top_level_str(manifest: &PluginManifest, key: &str) -> String {
    manifest
        .other
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// The MCP tool/resource/prompt contributions across both halves, each tagged
/// with the half that declares it.
fn mcp_contributions(manifest: &PluginManifest) -> McpDto {
    let mut out = McpDto {
        tools: Vec::new(),
        resources: Vec::new(),
        prompts: Vec::new(),
    };
    let halves = [
        ("agent", manifest.agent.as_ref().map(|a| &a.extra)),
        ("gcs", manifest.gcs.as_ref().map(|g| &g.extra)),
    ];
    for (half, extra) in halves {
        let Some(contributes) = extra.and_then(|e| e.get("contributes")) else {
            continue;
        };
        for (key, list) in [
            ("tools", &mut out.tools),
            ("resources", &mut out.resources),
            ("prompts", &mut out.prompts),
        ] {
            let Some(Value::Array(items)) = contributes
                .get(key)
                .and_then(|v| serde_json::to_value(v).ok())
            else {
                continue;
            };
            for item in items {
                if let Value::Object(mut map) = item {
                    map.insert("half".to_string(), Value::String(half.to_string()));
                    list.push(Value::Object(map));
                }
            }
        }
    }
    out
}

/// The installed manifest, or the refusal the detail routes answer with.
fn installed_manifest(sup: &PluginSupervisor, plugin_id: &str) -> Result<PluginManifest, Refusal> {
    sup.installed_manifest(plugin_id)
        .map_err(|e| Refusal::host_io(e.0))
}

/// `GET /api/plugins/{plugin_id}`: the install record, the granted capability
/// ids, and the manifest summary with the GCS block and MCP contributions.
pub async fn get_plugin(
    State(state): State<AppState>,
    AxumPath(plugin_id): AxumPath<String>,
) -> Response {
    respond(
        state
            .plugins
            .read(move |sup| {
                let install = sup
                    .find_install(&plugin_id)
                    .ok_or_else(|| Refusal::not_installed(&plugin_id))?;
                let manifest = installed_manifest(sup, &plugin_id)?;
                let permission_ids: Vec<String> =
                    manifest.declared_permissions().into_iter().collect();
                let granted_capabilities = install
                    .permissions
                    .iter()
                    .filter(|(_, g)| g.granted)
                    .map(|(id, _)| id.clone())
                    .collect();
                Ok(DetailDto {
                    install: InstallDto::from(install),
                    granted_capabilities,
                    manifest: ManifestDto {
                        id: manifest.id.clone(),
                        version: manifest.version.clone(),
                        name: manifest.name.clone(),
                        risk: manifest.risk.clone(),
                        license: top_level_str(&manifest, "license"),
                        halves: halves(&manifest),
                        gcs: gcs_block_json(&manifest),
                        permissions: enrich_permissions(&permission_ids, Some(&manifest), sup),
                        mcp: mcp_contributions(&manifest),
                    },
                })
            })
            .await
            .map(|body| json_response(StatusCode::OK, &body)),
    )
}

/// Resolve `asset` under `gcs_root`, refusing anything that could escape it: an
/// absolute path or a `..` component lexically, then a symlink that resolves
/// outside once canonicalized. `Ok(None)` is a path that does not exist or is
/// not a regular file.
fn resolve_gcs_asset(gcs_root: &Path, asset: &str) -> Result<Option<PathBuf>, Refusal> {
    let escapes = || {
        Refusal::new(
            12,
            "manifest_invalid",
            "gcs asset path escapes the plugin dir",
            StatusCode::BAD_REQUEST,
        )
    };
    let relative = Path::new(asset);
    if relative
        .components()
        .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err(escapes());
    }
    let Ok(root) = gcs_root.canonicalize() else {
        return Ok(None);
    };
    let Ok(target) = root.join(relative).canonicalize() else {
        return Ok(None);
    };
    if !target.starts_with(&root) {
        return Err(escapes());
    }
    Ok(target.is_file().then_some(target))
}

/// The content type a static asset is served with, by extension.
fn content_type_for(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match ext.as_str() {
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "html" | "htm" => "text/html; charset=utf-8",
        "json" | "map" => "application/json",
        "wasm" => "application/wasm",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "txt" => "text/plain; charset=utf-8",
        "yaml" | "yml" => "text/yaml; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// `GET /api/plugins/{plugin_id}/gcs/{*asset_path}`: one file from an installed
/// plugin's unpacked `gcs/` dir, so a LAN-paired GCS mounts the GCS half with
/// no cloud. Path-contained: `..`, absolute paths and outward symlinks are
/// refused, and only regular files are served.
pub async fn get_gcs_asset(
    State(state): State<AppState>,
    AxumPath((plugin_id, asset_path)): AxumPath<(String, String)>,
) -> Response {
    let result = state
        .plugins
        .read(move |sup| {
            if sup.find_install(&plugin_id).is_none() || !is_plugin_id(&plugin_id) {
                return Err(Refusal::not_installed(&plugin_id));
            }
            let gcs_root = sup.paths().install_dir.join(&plugin_id).join("gcs");
            let target = resolve_gcs_asset(&gcs_root, &asset_path)?
                .ok_or_else(|| Refusal::not_found(format!("gcs asset {asset_path:?} not found")))?;
            let body = std::fs::read(&target)
                .map_err(|e| Refusal::host_io(format!("read of gcs asset failed: {e}")))?;
            Ok((content_type_for(&target), body))
        })
        .await;
    respond(result.map(|(content_type, body)| {
        (StatusCode::OK, [(header::CONTENT_TYPE, content_type)], body).into_response()
    }))
}

/// `GET /api/plugins/{plugin_id}/manifest`: the installed `manifest.yaml`
/// byte for byte (refused when it no longer matches the recorded hash), so an
/// inline GCS module can check it against the attestation.
pub async fn get_manifest(
    State(state): State<AppState>,
    AxumPath(plugin_id): AxumPath<String>,
) -> Response {
    let result = state
        .plugins
        .read(move |sup| {
            if sup.find_install(&plugin_id).is_none() {
                return Err(Refusal::not_installed(&plugin_id));
            }
            sup.installed_manifest_bytes(&plugin_id)
                .map_err(|e| Refusal::host_io(e.0))
        })
        .await;
    respond(result.map(|bytes| {
        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/yaml; charset=utf-8")],
            bytes,
        )
            .into_response()
    }))
}

/// `GET /api/plugins/{plugin_id}/attestation`: `{signature, files: [{path,
/// sha256, payload?}]}`, the archive `SIGNATURE` entry and every attested file's
/// digest (payloads marked), written at install.
pub async fn get_attestation(
    State(state): State<AppState>,
    AxumPath(plugin_id): AxumPath<String>,
) -> Response {
    let result = state
        .plugins
        .read(move |sup| {
            if sup.find_install(&plugin_id).is_none() || !is_plugin_id(&plugin_id) {
                return Err(Refusal::not_installed(&plugin_id));
            }
            let dir = sup.paths().install_dir.join(&plugin_id);
            if !Attestation::path_in(&dir).exists() {
                return Err(Refusal::not_found(format!(
                    "plugin {plugin_id} has no install attestation; reinstall it"
                )));
            }
            sup.attestation(&plugin_id)
                .map_err(|e| Refusal::host_io(e.0))
        })
        .await;
    respond(result.map(|att| json_response(StatusCode::OK, &att)))
}

#[derive(Serialize)]
struct ReadinessDto {
    ok: bool,
    plugin_id: String,
    services: Value,
}

/// `GET /api/plugins/{plugin_id}/readiness`: `{ok, plugin_id, services}`, each
/// declared service that runs on this node probed now (empty when none).
pub async fn get_readiness(
    State(state): State<AppState>,
    AxumPath(plugin_id): AxumPath<String>,
) -> Response {
    let result = state
        .plugins
        .read(move |sup| {
            if sup.find_install(&plugin_id).is_none() {
                return Err(Refusal::not_installed(&plugin_id));
            }
            let services = sup
                .service_readiness(&plugin_id)
                .map_err(|e| {
                    if e.0.contains("not installed") {
                        Refusal::not_found(e.0)
                    } else {
                        Refusal::host_io(e.0)
                    }
                })?
                .unwrap_or_else(|| Value::Array(Vec::new()));
            Ok(ReadinessDto {
                ok: true,
                plugin_id,
                services,
            })
        })
        .await;
    respond(result.map(|body| json_response(StatusCode::OK, &body)))
}
