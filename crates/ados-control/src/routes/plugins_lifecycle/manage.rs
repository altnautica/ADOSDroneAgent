//! The lifecycle writes on an installed plugin: grant, revoke, enable, disable,
//! remove, and the auto-update preferences (pin, unpin, auto-update).

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use http_body_util::BodyExt;
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use ados_plugin_host::supervisor::PluginSupervisor;

use super::install::parse_body;
use super::{json_response, query_bool, respond, Refusal};
use crate::state::AppState;

/// How long enable waits for the residual vision service to resolve a
/// plugin's declared models (a cache hit is immediate; a first fetch downloads).
const MODEL_DELIVERY_TIMEOUT: Duration = Duration::from_secs(300);

/// Load the state and require `plugin_id` to be installed. A fresh supervisor
/// holds no installs until it reads the state file.
fn require_installed(sup: &mut PluginSupervisor, plugin_id: &str) -> Result<(), Refusal> {
    sup.refresh().map_err(|e| Refusal::host_io(e.to_string()))?;
    if sup.find_install(plugin_id).is_none() {
        return Err(Refusal::not_installed(plugin_id));
    }
    Ok(())
}

#[derive(Serialize)]
struct OkBody {
    ok: bool,
}

fn ok() -> Response {
    json_response(StatusCode::OK, &OkBody { ok: true })
}

#[derive(Deserialize)]
struct GrantRequest {
    permission_id: String,
}

/// `POST /api/plugins/{plugin_id}/grant`: `{permission_id}`. A permission the
/// manifest does not declare is `11 permission_deny`.
pub async fn grant_permission(
    State(state): State<AppState>,
    AxumPath(plugin_id): AxumPath<String>,
    body: Bytes,
) -> Response {
    let req: GrantRequest = match parse_body(&body) {
        Ok(r) => r,
        Err(bad) => return bad.into_response(),
    };
    let result = state
        .plugins
        .write(move |sup| {
            require_installed(sup, &plugin_id)?;
            sup.grant_permission(&plugin_id, &req.permission_id)
                .map_err(|e| {
                    let msg = e.to_string();
                    if msg.contains("did not declare") || msg.contains("not declared") {
                        Refusal::new(11, "permission_deny", msg, StatusCode::BAD_REQUEST)
                    } else {
                        Refusal::from_operation(&e)
                    }
                })
        })
        .await
        .and_then(|r| r);
    respond(result.map(|()| ok()))
}

#[derive(Serialize)]
struct RevokeResponse {
    ok: bool,
    plugin_id: String,
    granted: Vec<String>,
    requires_restart: bool,
}

/// `DELETE /api/plugins/{plugin_id}/perms/{permission_id}`: revoke, answering
/// the permissions still granted. Takes effect at once (the unit and the
/// plugin's token are refreshed), so no restart is needed.
pub async fn revoke_permission(
    State(state): State<AppState>,
    AxumPath((plugin_id, permission_id)): AxumPath<(String, String)>,
) -> Response {
    let result = state
        .plugins
        .write(move |sup| {
            require_installed(sup, &plugin_id)?;
            sup.revoke_permission(&plugin_id, &permission_id)
                .map_err(|e| Refusal::from_operation(&e))?;
            let granted = sup
                .find_install(&plugin_id)
                .map(|i| {
                    i.permissions
                        .iter()
                        .filter(|(_, g)| g.granted)
                        .map(|(id, _)| id.clone())
                        .collect()
                })
                .unwrap_or_default();
            Ok(RevokeResponse {
                ok: true,
                plugin_id,
                granted,
                requires_restart: false,
            })
        })
        .await
        .and_then(|r| r);
    respond(result.map(|body| json_response(StatusCode::OK, &body)))
}

/// Ask the residual vision service to resolve and cache the models `plugin_id`
/// declares (`POST /api/vision/plugin-models/{id}/deliver` on its internal
/// socket). Returns the per-model status list when the plugin declares any;
/// `None` when it declares none or the service could not be reached, which
/// never fails the enable.
async fn deliver_models(socket: &Path, plugin_id: &str) -> Option<Value> {
    let exchange = async {
        let stream = tokio::net::UnixStream::connect(socket).await.ok()?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .ok()?;
        let driver = tokio::spawn(async move {
            let _ = conn.await;
        });
        let request = http::Request::builder()
            .method(http::Method::POST)
            .uri(format!("/api/vision/plugin-models/{plugin_id}/deliver"))
            .header(http::header::HOST, "localhost")
            .header(http::header::CONTENT_LENGTH, "0")
            .body(Body::empty())
            .ok()?;
        let response = sender.send_request(request).await.ok()?;
        let status = response.status();
        let body = response.into_body().collect().await.ok()?.to_bytes();
        driver.abort();
        if !status.is_success() {
            tracing::warn!(plugin_id, %status, "plugin_model_delivery_refused");
            return None;
        }
        let doc: Value = serde_json::from_slice(&body).ok()?;
        doc.get("models")
            .filter(|m| m.as_array().is_some_and(|a| !a.is_empty()))
            .cloned()
    };
    match tokio::time::timeout(MODEL_DELIVERY_TIMEOUT, exchange).await {
        Ok(models) => models,
        Err(_) => {
            tracing::warn!(plugin_id, "plugin_model_delivery_timed_out");
            None
        }
    }
}

/// `POST /api/plugins/{plugin_id}/enable`: enable and start. Then, best-effort,
/// the models the plugin declares are delivered through the residual vision
/// service and their status recorded on the install, so the plugin finds its
/// model on first call and the GCS sees what is still missing.
pub async fn enable_plugin(
    State(state): State<AppState>,
    AxumPath(plugin_id): AxumPath<String>,
) -> Response {
    let id = plugin_id.clone();
    let enabled = state
        .plugins
        .write(move |sup| {
            require_installed(sup, &id)?;
            sup.enable(&id).map_err(|e| Refusal::from_operation(&e))
        })
        .await
        .and_then(|r| r);
    if let Err(r) = enabled {
        return respond(Err(r));
    }
    let socket = state.plugins.model_delivery_socket();
    if let Some(models) = deliver_models(&socket, &plugin_id).await {
        let total = models.as_array().map_or(0, Vec::len);
        let id = plugin_id.clone();
        let recorded = state
            .plugins
            .write(move |sup| sup.set_model_status(&id, models))
            .await;
        match recorded {
            Ok(Ok(())) => tracing::info!(plugin_id, total, "plugin_models_delivered"),
            Ok(Err(e)) => tracing::warn!(plugin_id, error = %e, "plugin_model_status_save_failed"),
            Err(r) => {
                tracing::warn!(plugin_id, error = %r.detail, "plugin_model_status_save_failed")
            }
        }
    }
    ok()
}

/// `POST /api/plugins/{plugin_id}/disable`: stop and disable.
pub async fn disable_plugin(
    State(state): State<AppState>,
    AxumPath(plugin_id): AxumPath<String>,
) -> Response {
    let result = state
        .plugins
        .write(move |sup| {
            require_installed(sup, &plugin_id)?;
            sup.disable(&plugin_id)
                .map_err(|e| Refusal::from_operation(&e))
        })
        .await
        .and_then(|r| r);
    respond(result.map(|()| ok()))
}

/// `DELETE /api/plugins/{plugin_id}?keep_data=`: disable, remove the units and
/// files, and (unless `keep_data`) the plugin's log.
pub async fn remove_plugin(
    State(state): State<AppState>,
    AxumPath(plugin_id): AxumPath<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let keep_data = match query.get("keep_data").map(|v| query_bool(v)) {
        None => false,
        Some(Some(b)) => b,
        Some(None) => {
            return crate::routes::detail(
                StatusCode::UNPROCESSABLE_ENTITY,
                "keep_data must be a boolean",
            )
        }
    };
    let result = state
        .plugins
        .write(move |sup| {
            require_installed(sup, &plugin_id)?;
            sup.remove(&plugin_id, keep_data)
                .map_err(|e| Refusal::from_operation(&e))
        })
        .await
        .and_then(|r| r);
    respond(result.map(|()| ok()))
}

#[derive(Serialize)]
struct PinResponse {
    ok: bool,
    plugin_id: String,
    pinned_version: Option<String>,
}

#[derive(Deserialize)]
struct PinRequest {
    version: String,
}

/// Record the version auto-update holds a plugin at (`None` lifts the pin).
async fn write_pin(state: &AppState, plugin_id: String, version: Option<String>) -> Response {
    let result = state
        .plugins
        .write(move |sup| {
            require_installed(sup, &plugin_id)?;
            sup.set_pinned_version(&plugin_id, version.clone())
                .map_err(|e| Refusal::from_operation(&e))?;
            Ok(PinResponse {
                ok: true,
                plugin_id,
                pinned_version: version,
            })
        })
        .await
        .and_then(|r| r);
    respond(result.map(|body| json_response(StatusCode::OK, &body)))
}

/// `POST /api/plugins/{plugin_id}/pin`: `{version}`. Auto-update keeps the
/// plugin at this version.
pub async fn pin_plugin(
    State(state): State<AppState>,
    AxumPath(plugin_id): AxumPath<String>,
    body: Bytes,
) -> Response {
    let req: PinRequest = match parse_body(&body) {
        Ok(r) => r,
        Err(bad) => return bad.into_response(),
    };
    let version = req.version.trim().to_string();
    if version.is_empty() {
        return respond(Err(Refusal::usage("usage_error", "version required")));
    }
    write_pin(&state, plugin_id, Some(version)).await
}

/// `POST /api/plugins/{plugin_id}/unpin`: lift the pin.
pub async fn unpin_plugin(
    State(state): State<AppState>,
    AxumPath(plugin_id): AxumPath<String>,
) -> Response {
    write_pin(&state, plugin_id, None).await
}

#[derive(Deserialize)]
struct AutoUpdateRequest {
    enabled: bool,
}

#[derive(Serialize)]
struct AutoUpdateResponse {
    ok: bool,
    plugin_id: String,
    auto_update: bool,
}

/// `POST /api/plugins/{plugin_id}/auto-update`: `{enabled}`. Whether the
/// cloud relay's daily check may update this plugin.
pub async fn set_auto_update(
    State(state): State<AppState>,
    AxumPath(plugin_id): AxumPath<String>,
    body: Bytes,
) -> Response {
    let req: AutoUpdateRequest = match parse_body(&body) {
        Ok(r) => r,
        Err(bad) => return bad.into_response(),
    };
    let result = state
        .plugins
        .write(move |sup| {
            require_installed(sup, &plugin_id)?;
            sup.set_auto_update(&plugin_id, req.enabled)
                .map_err(|e| Refusal::from_operation(&e))?;
            Ok(AutoUpdateResponse {
                ok: true,
                plugin_id,
                auto_update: req.enabled,
            })
        })
        .await
        .and_then(|r| r);
    respond(result.map(|body| json_response(StatusCode::OK, &body)))
}
