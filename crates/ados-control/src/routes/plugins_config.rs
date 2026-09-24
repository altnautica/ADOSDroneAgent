//! The plugin per-drone config routes: `PUT` and `GET /api/plugins/{plugin_id}/config`.
//!
//! A GCS skill toggle (the Fly Mode Skill Bar flipping a behavior's `active`
//! flag) and a per-drone settings change (a follow distance/height edit) both
//! write a plugin's per-drone config. The plugin reads that config each tick
//! from the LIVE in-memory store in the running `ados-plugin-host`, so the
//! write must reach that daemon — a disk write alone is not seen until restart.
//! This route is that reach: it forwards `{key, value, scope?}` to the daemon's
//! on-box control socket via [`PluginControlClient`].
//!
//! This is the RUST-FIRST home for the write (a native `ados-control` route, not
//! a residual-Python plugin route): config writes are a control-plane operation,
//! so they stay off the FastAPI surface. The plugin *read* routes (`GET
//! /api/plugins/{id}`, `GET /api/plugins/{id}/gcs/...`) remain Python for now (a
//! later route-ledger wave).
//!
//! The `GET` reads the same live store back (global keys with this drone's own
//! keys over them, exactly what the plugin reads), so the GCS can hand a
//! plugin's UI its current settings after a reload.
//!
//! Auth: this is a write, so it sits in the native route set and the LAN edge
//! requires the pairing key when paired (the same posture as `/api/command` and
//! `/api/vision/designate`); an unreachable daemon is a 503, never a silent drop.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::ipc::plugin_control_client::PluginControlError;
use crate::ipc::PluginControlClient;
use crate::routes::detail;
use crate::state::AppState;

/// The `PUT /api/plugins/{plugin_id}/config` body. `value` is any JSON value
/// (a bool for a skill toggle, a number for a follow distance), deserialized
/// straight into an `rmpv::Value` so it round-trips to the daemon losslessly.
#[derive(Debug, Deserialize)]
pub struct PutConfigBody {
    /// The config key to write (e.g. `active`, `follow_distance_m`).
    pub key: String,
    /// The new value. Any JSON scalar/array/object.
    pub value: rmpv::Value,
    /// `drone` (per-drone, the default) or `global`. Absent/empty → drone.
    #[serde(default)]
    pub scope: Option<String>,
}

/// Pull the effective scope string out of the daemon's `{set, scope}` response.
fn scope_of(args: &rmpv::Value) -> Option<String> {
    match args {
        rmpv::Value::Map(m) => m
            .iter()
            .find(|(k, _)| k.as_str() == Some("scope"))
            .and_then(|(_, v)| v.as_str())
            .map(|s| s.to_string()),
        _ => None,
    }
}

/// `PUT /api/plugins/{plugin_id}/config` — write a plugin's per-drone config to
/// the live plugin host.
pub async fn put_plugin_config(
    State(_state): State<AppState>,
    Path(plugin_id): Path<String>,
    Json(body): Json<PutConfigBody>,
) -> Response {
    if body.key.trim().is_empty() {
        return detail(StatusCode::BAD_REQUEST, "key must be a non-empty string");
    }
    let client = PluginControlClient::default_socket();
    match client
        .config_set(&plugin_id, &body.key, body.value, body.scope.as_deref())
        .await
    {
        Ok(resp) => (
            StatusCode::OK,
            Json(json!({
                "set": true,
                "plugin_id": plugin_id,
                "key": body.key,
                "scope": scope_of(&resp),
            })),
        )
            .into_response(),
        // A bad request the daemon rejected (empty key, bad scope) is a 400; an
        // unreachable daemon (plugin host not up) is a 503 — a config write is
        // never silently dropped.
        Err(PluginControlError::Rpc(msg)) => detail(StatusCode::BAD_REQUEST, msg),
        Err(PluginControlError::Timeout) => detail(
            StatusCode::GATEWAY_TIMEOUT,
            "plugin host did not answer in time",
        ),
        Err(e) => detail(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("plugin host unavailable: {e}"),
        ),
    }
}

/// `GET /api/plugins/{plugin_id}/config` — the plugin's effective per-drone
/// config from the live plugin host, `{plugin_id, values: {key: value}}`.
pub async fn get_plugin_config(
    State(_state): State<AppState>,
    Path(plugin_id): Path<String>,
) -> Response {
    let client = PluginControlClient::default_socket();
    match client.config_get(&plugin_id).await {
        Ok(values) => {
            let values = serde_json::to_value(&values).unwrap_or(json!({}));
            (
                StatusCode::OK,
                Json(json!({ "plugin_id": plugin_id, "values": values })),
            )
                .into_response()
        }
        Err(PluginControlError::Rpc(msg)) => detail(StatusCode::BAD_REQUEST, msg),
        Err(PluginControlError::Timeout) => detail(
            StatusCode::GATEWAY_TIMEOUT,
            "plugin host did not answer in time",
        ),
        Err(e) => detail(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("plugin host unavailable: {e}"),
        ),
    }
}
