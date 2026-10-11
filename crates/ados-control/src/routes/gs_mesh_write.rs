//! Ground-station mesh write routes (role, gateway preference, mesh config).
//!
//! The ground-station profile exposes its mesh write surface under
//! `/api/v1/ground-station/{role,mesh/gateway_preference,mesh/config}`. The
//! matching reads live in [`crate::routes::gs_mesh`].
//!
//! ## What ports here, and how
//!
//! - **`PUT /role`** — change the mesh role. The validation gates (profile,
//!   mesh-capability, relay-must-be-paired, valid role) run here against the same
//!   on-disk files the FastAPI route reads, then the transition itself
//!   (stop/start + mask/unmask of the role units, the sentinel flip, the
//!   `role_changed` event) is forwarded to the supervisor control socket's
//!   `set_role` op. The supervisor executes it because it is the one process no
//!   transition stops; the data plane's own socket lives inside a role unit. The
//!   reply carries the transition metadata (`role`/`previous`/`units_started`/
//!   `units_stopped`/`ts_ms`/`noop`). A best-effort `ground_station.role` config
//!   persist follows, so the configured role the supervisor applies at boot
//!   matches.
//! - **`PUT /mesh/gateway_preference`** — pin a gateway / let batman auto-pick /
//!   disable client mode. Forwarded to the `set_gateway_preference` op, which
//!   persists `/etc/ados/mesh/gateway.json` and drives `batctl`. The route gates
//!   on the profile and the not-`direct` role first (the FastAPI 404
//!   `E_NOT_IN_MESH`), then maps the op reply to the FastAPI response body
//!   (`{mode,pinned_mac,persisted[,persist_error]}`) or the 503 `E_BATCTL_UNAVAILABLE`.
//! - **`PUT /mesh/config`** — set the configured mesh transport fields
//!   (`mesh_id`/`carrier`/`channel`). Its whole effect is a config-file persist
//!   under `ground_station.mesh`, so it is a surgical config merge the front does
//!   directly (the same approach the GS-wfb config write takes), no command-socket
//!   op needed. Returns `{mesh_id,carrier,channel,applied}` projected through the
//!   config-model defaults.
//!
//! The mesh WebSocket routes (`/ws/mesh`, `/ws/uplink`) stay on the residual
//! surface.
//!
//! ## Error-body shape
//!
//! Every gate raises an `HTTPException(detail={"error": {...}})` on the FastAPI
//! side, so FastAPI renders `{"detail": {"error": {...}}}` — the nested object
//! shape, NOT the bare-string `{"detail"}`. The helpers here build that exact
//! shape so the front is byte-identical (the same convention the GS mesh-read
//! module uses).

use std::path::{Path, PathBuf};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use ados_supervisor::config::VALID_ROLES;

use crate::config_store::{section, section_path, update_config};
use crate::routes::gs_cmd::groundlink_cmd_roundtrip;
use crate::state::AppState;

mod merge;
mod mesh_config;

use merge::*;
pub use mesh_config::put_mesh_config;
pub use mesh_config::MeshConfigUpdate;
#[cfg(test)]
use mesh_config::*;

/// How long the role write waits for the supervisor's reply: the transition
/// stops and starts up to four systemd units, each bounded by the supervisor's
/// own `systemctl` ceiling, and it waits behind an in-flight monitor pass.
const ROLE_TRANSITION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

// ---------------------------------------------------------------------------
// Profile + role gating (mirrors the FastAPI `_require_ground_profile` +
// `role_manager.get_current_role`, byte-identical to the read module).
// ---------------------------------------------------------------------------

/// The carrier-config default, matching the Python `MeshConfig.carrier` default.
const DEFAULT_CARRIER: &str = "802.11s";

/// The mesh-channel config default, matching the Python `MeshConfig.channel`.
const DEFAULT_MESH_CHANNEL: i64 = 1;

/// Build a `(status, {"detail": {"error": <error>}})` response. FastAPI wraps an
/// `HTTPException(detail={"error": {...}})` as `{"detail": {"error": {...}}}`, so
/// the native body matches byte-for-byte. The read module builds the same shape.
fn nested_detail(status: StatusCode, error: Value) -> Response {
    (status, Json(json!({ "detail": { "error": error } }))).into_response()
}

/// The FastAPI profile-mismatch 404: a `detail` carrying the `E_PROFILE_MISMATCH`
/// error object. A drone-profile caller hits every ground-station route with this.
fn profile_mismatch() -> Response {
    nested_detail(StatusCode::NOT_FOUND, json!({"code": "E_PROFILE_MISMATCH"}))
}

/// True when the resolved profile is a ground station. Resolves through the
/// shared profile module (config `agent.profile` + the on-disk sentinels), the
/// same source of truth the node advertises on the wire, mirroring the Python
/// `is_ground_station`.
fn is_ground_station() -> bool {
    let cfg = crate::config::PairingConfig::load();
    let (profile, _role) = crate::profile::current_profile_and_role(&cfg.agent.profile);
    profile == "ground-station"
}

// ---------------------------------------------------------------------------
// Path seams.
// ---------------------------------------------------------------------------

/// The agent config path (`ADOS_CONFIG`, default `/etc/ados/config.yaml`), the
/// same resolution the sibling read/write routes use.
fn config_yaml_path() -> PathBuf {
    PathBuf::from(
        std::env::var("ADOS_CONFIG").unwrap_or_else(|_| crate::config::CONFIG_YAML.to_string()),
    )
}

/// The persistent mesh dir (`<mesh-role parent>`), so a test override of
/// `ADOS_MESH_ROLE` carries the identity/psk sentinels alongside it. In
/// production this resolves to `/etc/ados/mesh`.
fn mesh_dir() -> PathBuf {
    ados_config::mesh_role_path()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("/etc/ados/mesh"))
}

// ---------------------------------------------------------------------------
// Mesh-capability + paired-identity gates (the FastAPI role-route preconditions).
// ---------------------------------------------------------------------------

/// The mesh-capability gate for a role write: relay and receiver need two
/// WFB-class radios, `direct` is always allowed (the opt-out path). `capable`
/// is the live hardware probe ([`ados_protocol::netif::mesh_capable`] in
/// production), injected so a test can point it at a fake sysfs root.
fn mesh_capability_gate(role: &str, capable: impl FnOnce() -> bool) -> Option<Response> {
    if (role == "relay" || role == "receiver") && !capable() {
        return Some(nested_detail(
            StatusCode::CONFLICT,
            json!({"code": "E_MESH_NOT_CAPABLE"}),
        ));
    }
    None
}

/// The live mesh-capability probe, off the async runtime (it walks sysfs).
async fn mesh_capable_live() -> bool {
    tokio::task::spawn_blocking(ados_protocol::netif::mesh_capable)
        .await
        .unwrap_or(false)
}

/// True when the mesh identity (id + psk) is already on disk. Mirrors the Python
/// `has_persisted_identity` (`MESH_ID_PATH.is_file() and MESH_PSK_PATH.is_file()`)
/// the relay-role gate consults.
fn has_persisted_identity() -> bool {
    let dir = mesh_dir();
    dir.join("id").is_file() && dir.join("psk.key").is_file()
}

// ---------------------------------------------------------------------------
// PUT /api/v1/ground-station/role — change the mesh role.
// ---------------------------------------------------------------------------

/// The `PUT .../role` request body: `role` plus an optional `confirm_token` (currently unused by the
/// handler, but accepted so an old client's body still deserializes). The FastAPI model types
/// `role` as a `Literal["direct","relay","receiver"]`, so an out-of-range value is a 422 there; the
/// front has no such pre-validation, so an unknown role reaches the handler and is rejected with
/// the FastAPI 400 `E_INVALID_ROLE` the in-process `apply_role`'s `ValueError` would surface.
#[derive(Debug, Default, Deserialize)]
pub struct RoleChangeRequest {
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub confirm_token: Option<String>,
}

/// `PUT .../role` → the role-transition metadata.
///
/// Gates (in the FastAPI order): profile (404 `E_PROFILE_MISMATCH`), valid role
/// (400 `E_INVALID_ROLE`), mesh-capability for relay/receiver (409
/// `E_MESH_NOT_CAPABLE`), relay-must-be-paired (409 `E_NOT_PAIRED`). Then forwards
/// the `set_role` op to the data-plane command socket and returns its
/// `{role,previous,units_started,units_stopped,ts_ms,noop}` reply. A best-effort
/// `ground_station.role` config merge follows (mirroring the FastAPI post-apply
/// save), never affecting the response.
pub async fn put_role(
    State(_state): State<AppState>,
    Json(req): Json<RoleChangeRequest>,
) -> Response {
    if !is_ground_station() {
        return profile_mismatch();
    }
    let role = match req.role.as_deref() {
        Some(r) if VALID_ROLES.contains(&r) => r.to_string(),
        other => {
            // The FastAPI `Literal` rejects an out-of-range role with a 422 before
            // the handler; the front mirrors the in-process `apply_role`
            // ValueError → 400 `E_INVALID_ROLE` for an unknown value (it has no
            // Pydantic pre-validation). A `null` role is treated the same way.
            let got = other.unwrap_or("");
            return nested_detail(
                StatusCode::BAD_REQUEST,
                json!({
                    "code": "E_INVALID_ROLE",
                    "message": format!(
                        "role must be one of {VALID_ROLES:?}, got {got:?}"
                    ),
                }),
            );
        }
    };

    // Mesh-capability gate: relay/receiver need two WFB-class radios, detected
    // live; direct is always allowed. The probe only runs when the role needs it.
    let capable = if role == "relay" || role == "receiver" {
        mesh_capable_live().await
    } else {
        true
    };
    if let Some(refused) = mesh_capability_gate(&role, || capable) {
        return refused;
    }

    // Paired-identity gate for relay: a fresh box with no mesh identity would send
    // the mesh manager into a restart loop, so force a pair first. `direct` /
    // `receiver` have no such requirement.
    if role == "relay" && !has_persisted_identity() {
        return nested_detail(
            StatusCode::CONFLICT,
            json!({
                "code": "E_NOT_PAIRED",
                "message": "relay role requires a completed pair with a receiver",
            }),
        );
    }

    // Forward the transition to the supervisor. It owns the systemctl
    // orchestration + the sentinel flip + the role event; its reply carries the
    // transition metadata.
    let request = json!({"op": "set_role", "role": role, "reason": "rest"});
    let reply = match supervisor_roundtrip(&request).await {
        Some(r) => r,
        None => return supervisor_unavailable(),
    };
    let result = match strip_ok(reply) {
        Ok(body) => body,
        Err(err) => {
            // The supervisor refused the transition: an unknown role (which the
            // route pre-validated), a node that is not a ground station, or a
            // bind that owns the radio adapter.
            return socket_error_to_response(err);
        }
    };

    // Persist `ground_station.role` so the value survives a reboot even if the
    // sentinel is wiped. The role is already applied, so a persist failure does
    // not fail the request, but the body says whether it will survive a restart.
    let mut result = result;
    match merge_ground_station_role(&config_yaml_path(), &role) {
        Ok(()) => {
            result.insert("persisted".to_string(), json!(true));
        }
        Err(e) => {
            tracing::error!(error = %e, role = %role, "ground station role applied but not persisted");
            result.insert("persisted".to_string(), json!(false));
            result.insert("persist_error".to_string(), json!(e));
        }
    }

    Json(Value::Object(result)).into_response()
}

// ---------------------------------------------------------------------------
// PUT /api/v1/ground-station/mesh/gateway_preference — gateway pin / auto / off.
// ---------------------------------------------------------------------------

/// The `PUT .../mesh/gateway_preference` body: a required `mode` (`auto`/`pinned`/`off`) and an
/// optional `pinned_mac`. The Pydantic `Literal` rejects an out-of-range mode with a 422; the front
/// validates the mode here (the socket op rejects it too) and surfaces a 422-equivalent 400 only
/// via the command-socket's own validation, which is not reachable for a typed-good mode. An
/// unknown mode is forwarded and the socket's `E_INVALID_MODE` maps to a 400.
#[derive(Debug, Default, Deserialize)]
pub struct MeshGatewayPreferenceUpdate {
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub pinned_mac: Option<String>,
}

/// `PUT .../mesh/gateway_preference` → `{mode, pinned_mac, persisted[, persist_error]}`.
///
/// Gates on the profile (404 `E_PROFILE_MISMATCH`) and the not-`direct` role (404
/// `E_NOT_IN_MESH`, matching the FastAPI `get_current_role() == "direct"` guard),
/// then forwards the `set_gateway_preference` op (which persists
/// `gateway.json` + drives `batctl`) and maps its reply to the FastAPI response.
/// A reply carrying `E_BATCTL_UNAVAILABLE` becomes the FastAPI 503; an absent
/// socket becomes a 503 too (the front cannot drive `batctl` itself).
pub async fn put_gateway_preference(
    State(_state): State<AppState>,
    Json(update): Json<MeshGatewayPreferenceUpdate>,
) -> Response {
    if !is_ground_station() {
        return profile_mismatch();
    }
    // The not-in-mesh gate: a `direct` node is not in a mesh, so the FastAPI route
    // 404s with `E_NOT_IN_MESH` before touching batman. Resolved from the role
    // sentinel, the same source the read module uses.
    if ados_config::read_mesh_role(&ados_config::mesh_role_path()) == "direct" {
        return nested_detail(StatusCode::NOT_FOUND, json!({"code": "E_NOT_IN_MESH"}));
    }

    let mode = update.mode.clone().unwrap_or_default();
    let request = json!({
        "op": "set_gateway_preference",
        "mode": mode,
        "pinned_mac": update.pinned_mac,
    });
    let reply = match groundlink_cmd_roundtrip(&request).await {
        Some(r) => r,
        // The FastAPI route's reachable failure here is only the absent-batctl
        // 503; an unreachable socket is the front's no-link posture, also a 503.
        None => {
            return nested_detail(
                StatusCode::SERVICE_UNAVAILABLE,
                json!({"code": "E_BATCTL_UNAVAILABLE"}),
            )
        }
    };

    match strip_ok(reply) {
        Ok(body) => Json(Value::Object(body)).into_response(),
        Err(err) => {
            // The socket maps an absent `batctl` to `E_BATCTL_UNAVAILABLE`; the
            // FastAPI route raises that as a 503. An `E_INVALID_MODE` (an
            // out-of-range mode the Pydantic Literal would 422) maps to a 400.
            match err.code.as_str() {
                "E_BATCTL_UNAVAILABLE" => nested_detail(
                    StatusCode::SERVICE_UNAVAILABLE,
                    json!({"code": "E_BATCTL_UNAVAILABLE"}),
                ),
                _ => socket_error_to_response(err),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Command-socket reply mapping.
// ---------------------------------------------------------------------------

/// A command-socket failure reply: the apply-time `error` code + an optional
/// `message`. The route maps each to the FastAPI 4xx/5xx body.
#[derive(Debug)]
pub struct SocketError {
    pub code: String,
    pub message: Option<String>,
}

/// Split a command-socket reply on its transport `ok` flag. `ok:true` (or absent)
/// yields the reply object with the `ok` key stripped (the body the route returns
/// verbatim); `ok:false` yields the [`SocketError`] the route maps to a status.
fn strip_ok(reply: Value) -> Result<Map<String, Value>, SocketError> {
    let Value::Object(mut obj) = reply else {
        return Err(SocketError {
            code: "E_BAD_REPLY".to_string(),
            message: Some("command socket reply was not an object".to_string()),
        });
    };
    if obj.get("ok") == Some(&Value::Bool(false)) {
        let code = obj
            .get("error")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("E_COMMAND_FAILED")
            .to_string();
        let message = obj
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string);
        return Err(SocketError { code, message });
    }
    obj.remove("ok");
    Ok(obj)
}

/// Map a command-socket error to a FastAPI-shaped 400 body in the nested
/// `{"detail": {"error": {"code"[, "message"]}}}` shape. Used for the generic
/// failure arms (an unknown role, an invalid mode); the batctl-unavailable case is
/// mapped to a 503 by its caller before this.
fn socket_error_to_response(err: SocketError) -> Response {
    let mut error = Map::new();
    error.insert("code".to_string(), json!(err.code));
    if let Some(msg) = err.message {
        error.insert("message".to_string(), json!(msg));
    }
    nested_detail(StatusCode::BAD_REQUEST, Value::Object(error))
}

/// The supervisor control socket under the run dir (`ADOS_RUN_DIR`, default
/// `/run/ados`), the same resolution the supervisor binds it under.
pub(crate) fn supervisor_sock() -> PathBuf {
    PathBuf::from(std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string()))
        .join(ados_supervisor::bind::control::SUPERVISOR_SOCK_NAME)
}

/// One `set_role` exchange with the supervisor control socket: the parsed object
/// reply (its `ok` flag intact), or `None` when no object reply arrived within
/// [`ROLE_TRANSITION_TIMEOUT`].
async fn supervisor_roundtrip(request: &Value) -> Option<Value> {
    crate::ipc::cmd::roundtrip_object(&supervisor_sock(), request, ROLE_TRANSITION_TIMEOUT)
        .await
        .ok()
        .map(Value::Object)
}

/// The 503 when the supervisor control socket is unreachable: the front owns no
/// systemd lifecycle, so it cannot run the transition itself.
fn supervisor_unavailable() -> Response {
    nested_detail(
        StatusCode::SERVICE_UNAVAILABLE,
        json!({
            "code": "E_SUPERVISOR_UNAVAILABLE",
            "message": "supervisor control socket unavailable",
        }),
    )
}

#[cfg(test)]
mod tests;
