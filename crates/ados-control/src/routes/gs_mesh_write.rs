//! Ground-station mesh write routes (role, gateway preference, mesh config).
//!
//! The ground-station profile exposes its mesh write surface under
//! `/api/v1/ground-station/{role,mesh/gateway_preference,mesh/config}`. The
//! matching reads live in [`crate::routes::gs_mesh`]; this module serves the
//! three writes the front can reproduce faithfully now that the data-plane
//! service carries a command socket.
//!
//! ## What ports here, and how
//!
//! - **`PUT /role`** — change the mesh role. The validation gates (profile,
//!   mesh-capability, relay-must-be-paired, valid role) run here against the same
//!   on-disk files the FastAPI route reads, then the transition itself
//!   (stop/start + mask/unmask of the role-gated systemd units, the sentinel
//!   flip, the `role_changed` event) is forwarded to the data-plane command
//!   socket's `set_role` op. The socket reply carries the transition metadata the
//!   FastAPI route returned (`role`/`previous`/`units_started`/`units_stopped`/
//!   `ts_ms`/`noop`). A best-effort `ground_station.role` config persist follows,
//!   mirroring the FastAPI route's post-apply save.
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

use crate::routes::gs_cmd::groundlink_cmd_roundtrip;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// Profile + role gating (mirrors the FastAPI `_require_ground_profile` +
// `role_manager.get_current_role`, byte-identical to the read module).
// ---------------------------------------------------------------------------

/// The valid mesh roles, in advertised order. Mirrors `role_manager.VALID_ROLES`.
const VALID_ROLES: [&str; 3] = ["direct", "relay", "receiver"];

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

/// The profile-source sentinel (`ADOS_PROFILE_CONF`, default
/// `/etc/ados/profile.conf`), the same file the FastAPI mesh-capability gate
/// reads. install.sh writes it as YAML.
fn profile_conf_path() -> PathBuf {
    std::env::var("ADOS_PROFILE_CONF")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(crate::profile::PROFILE_CONF))
}

/// The persistent mesh dir (`<mesh-role parent>`), so a test override of
/// `ADOS_MESH_ROLE` carries the identity/psk sentinels alongside it. In
/// production this resolves to `/etc/ados/mesh`.
fn mesh_dir() -> PathBuf {
    let role_path = std::env::var("ADOS_MESH_ROLE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(crate::profile::MESH_ROLE_PATH));
    role_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("/etc/ados/mesh"))
}

// ---------------------------------------------------------------------------
// Mesh-capability + paired-identity gates (the FastAPI role-route preconditions).
// ---------------------------------------------------------------------------

/// True when `profile.conf` carries `mesh_capable: true`. The FastAPI route reads
/// `_read_yaml_or_empty(PROFILE_CONF).get("mesh_capable", False)`; a missing file
/// / key / a non-true value is not mesh-capable. Mirrors that read.
fn mesh_capable() -> bool {
    let Ok(text) = std::fs::read_to_string(profile_conf_path()) else {
        return false;
    };
    let Ok(doc) = serde_norway::from_str::<Value>(&text) else {
        return false;
    };
    doc.get("mesh_capable")
        .and_then(Value::as_bool)
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

/// The `PUT .../role` request body. Mirrors the FastAPI `RoleChangeRequest`:
/// `role` plus an optional `confirm_token` (currently unused by the handler, but
/// accepted so an old client's body still deserializes). The FastAPI model types
/// `role` as a `Literal["direct","relay","receiver"]`, so an out-of-range value
/// is a 422 there; the front has no such pre-validation, so an unknown role
/// reaches the handler and is rejected with the FastAPI 400 `E_INVALID_ROLE` the
/// in-process `apply_role`'s `ValueError` would surface.
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

    // Mesh-capability gate: relay/receiver require the `mesh_capable` flag; direct
    // is always allowed (the opt-out path even when the flag is absent).
    if (role == "relay" || role == "receiver") && !mesh_capable() {
        return nested_detail(StatusCode::CONFLICT, json!({"code": "E_MESH_NOT_CAPABLE"}));
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

    // Forward the transition to the data-plane command socket. The socket owns the
    // systemctl orchestration + the sentinel flip + the role event; its reply
    // carries the transition metadata the FastAPI route returned.
    let request = json!({"op": "set_role", "role": role, "reason": "rest"});
    let reply = match groundlink_cmd_roundtrip(&request).await {
        Some(r) => r,
        None => return socket_unavailable(),
    };
    let result = match strip_ok(reply) {
        Ok(body) => body,
        Err(err) => {
            // The socket reported a failure. The only failure the apply can return
            // is an unknown role, which the route already pre-validated, so this is
            // a belt-and-suspenders 400 with the socket's error code/message.
            return socket_error_to_response(err);
        }
    };

    // Best-effort persist of `ground_station.role` so the value survives a reboot
    // even if the sentinel is wiped, mirroring the FastAPI post-apply save (which
    // is wrapped in a try/except pass and never affects the response).
    let _ = merge_ground_station_role(&config_yaml_path(), &role);

    Json(Value::Object(result)).into_response()
}

// ---------------------------------------------------------------------------
// PUT /api/v1/ground-station/mesh/gateway_preference — gateway pin / auto / off.
// ---------------------------------------------------------------------------

/// The `PUT .../mesh/gateway_preference` body. Mirrors the FastAPI
/// `MeshGatewayPreferenceUpdate`: a required `mode` (`auto`/`pinned`/`off`) and an
/// optional `pinned_mac`. The Pydantic `Literal` rejects an out-of-range mode with
/// a 422; the front validates the mode here (the socket op rejects it too) and
/// surfaces a 422-equivalent 400 only via the command-socket's own validation,
/// which is not reachable for a typed-good mode. An unknown mode is forwarded and
/// the socket's `E_INVALID_MODE` maps to a 400.
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
    if current_role() == "direct" {
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
// PUT /api/v1/ground-station/mesh/config — set the configured mesh transport.
// ---------------------------------------------------------------------------

/// The `PUT .../mesh/config` body. Mirrors the FastAPI `MeshConfigUpdate`: three
/// optional fields, each applied only when present. `carrier` is a
/// `Literal["802.11s","ibss"]` on the FastAPI side and `channel` is `ge=1,le=13`;
/// the front mirrors the valid path byte-for-byte and leaves the out-of-range
/// rejection to the residual surface (a value outside those ranges is rare from
/// the GCS, which sends only the typed values).
#[derive(Debug, Default, Deserialize)]
pub struct MeshConfigUpdate {
    #[serde(default)]
    pub mesh_id: Option<String>,
    #[serde(default)]
    pub carrier: Option<String>,
    #[serde(default)]
    pub channel: Option<i64>,
}

/// `PUT .../mesh/config` → `{mesh_id, carrier, channel, applied}`.
///
/// Gates on the profile (404 `E_PROFILE_MISMATCH`), then surgically merges the
/// supplied fields into `ground_station.mesh` of the on-disk config (preserving
/// every other key — the same approach the GS-wfb config write takes) and returns
/// the resolved view (request → existing → config-model default) plus `applied`
/// (true iff at least one field was supplied). There is no command-socket op: the
/// mesh manager reads these from the same config file on its own cadence.
pub async fn put_mesh_config(
    State(_state): State<AppState>,
    Json(update): Json<MeshConfigUpdate>,
) -> Response {
    if !is_ground_station() {
        return profile_mismatch();
    }
    put_mesh_config_at(&config_yaml_path(), &update)
}

/// The merge logic against an explicit config path (a test points it at a temp
/// file). The response mirrors the FastAPI handler, which echoes the post-mutation
/// `mesh.{mesh_id,carrier,channel}` model values + `applied`.
fn put_mesh_config_at(config_path: &Path, update: &MeshConfigUpdate) -> Response {
    let (mesh_id, carrier, channel, applied) = merge_mesh_config(config_path, update);
    Json(json!({
        "mesh_id": mesh_id,
        "carrier": carrier,
        "channel": channel,
        "applied": applied,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Role sentinel + config-merge helpers.
// ---------------------------------------------------------------------------

/// Read the on-disk role sentinel, defaulting to `direct` when the file is
/// missing/unreadable/unknown. Mirrors `role_manager.get_current_role`.
fn current_role() -> String {
    let path = std::env::var("ADOS_MESH_ROLE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(crate::profile::MESH_ROLE_PATH));
    if let Ok(text) = std::fs::read_to_string(path) {
        let value = text.trim();
        if VALID_ROLES.contains(&value) {
            return value.to_string();
        }
    }
    "direct".to_string()
}

/// Merge `ground_station.role` into the on-disk config (atomic, preserving every
/// other key), the best-effort persist the FastAPI route does after a successful
/// `apply_role`. Returns the IO error string on a write fault (the caller ignores
/// it — the persist never affects the response, matching the FastAPI try/except).
fn merge_ground_station_role(config_path: &Path, role: &str) -> Result<(), String> {
    use serde_norway::Value as Yaml;
    let mut data: Yaml = load_or_empty_mapping(config_path);
    {
        let gs = section_path_mut(&mut data, &["ground_station"]).ok_or("config root not a map")?;
        gs.insert(
            Yaml::String("role".to_string()),
            Yaml::String(role.to_string()),
        );
    }
    write_atomic(config_path, &data)
}

/// Merge the supplied `ground_station.mesh` fields into the on-disk config and
/// return the resolved view values + whether anything was applied. Each view value
/// resolves the FastAPI way over the post-mutation model: the request value when
/// supplied, else the existing on-disk value, else the config-model default
/// (`mesh_id: null`, `carrier: "802.11s"`, `channel: 1`). `applied` is true iff at
/// least one field was supplied (the FastAPI `changed` flag).
fn merge_mesh_config(config_path: &Path, update: &MeshConfigUpdate) -> (Value, String, i64, bool) {
    use serde_norway::Value as Yaml;

    let mut data: Yaml = load_or_empty_mapping(config_path);
    let existing = existing_mesh(&data);

    let applied = update.mesh_id.is_some() || update.carrier.is_some() || update.channel.is_some();

    if applied {
        if let Some(mesh) = section_path_mut(&mut data, &["ground_station", "mesh"]) {
            if let Some(id) = &update.mesh_id {
                mesh.insert(
                    Yaml::String("mesh_id".to_string()),
                    Yaml::String(id.clone()),
                );
            }
            if let Some(c) = &update.carrier {
                mesh.insert(Yaml::String("carrier".to_string()), Yaml::String(c.clone()));
            }
            if let Some(ch) = update.channel {
                mesh.insert(Yaml::String("channel".to_string()), Yaml::Number(ch.into()));
            }
            // Persist is best-effort: a write fault still answers the resolved view
            // (the FastAPI route only saves when `changed`, swallowing failures).
            let _ = write_atomic(config_path, &data);
        }
    }

    // The resolved view: request → existing → config-model default. The FastAPI
    // route returns the post-mutation model fields, where an unset request field
    // keeps its on-disk value and an absent on-disk value reads the model default.
    let mesh_id = match &update.mesh_id {
        Some(id) => Value::String(id.clone()),
        None => existing.mesh_id,
    };
    let carrier = update
        .carrier
        .clone()
        .or(existing.carrier)
        .unwrap_or_else(|| DEFAULT_CARRIER.to_string());
    let channel = update
        .channel
        .or(existing.channel)
        .unwrap_or(DEFAULT_MESH_CHANNEL);

    (mesh_id, carrier, channel, applied)
}

/// The existing `ground_station.mesh` view fields already on disk. `mesh_id` is a
/// JSON value (so an absent one reads `null` like the Python `mesh_id: None`
/// default); `carrier`/`channel` are `None` when absent so the merge falls through
/// to the config-model default.
#[derive(Default)]
struct ExistingMesh {
    mesh_id: Value,
    carrier: Option<String>,
    channel: Option<i64>,
}

/// Read the existing `ground_station.mesh` view fields from a parsed config value.
/// An absent section reads `mesh_id: null` + no carrier/channel, so the resolved
/// view falls through to the config-model defaults — byte-identical to the FastAPI
/// route reading the default-constructed `MeshConfig`.
fn existing_mesh(data: &serde_norway::Value) -> ExistingMesh {
    let mesh = data.get("ground_station").and_then(|v| v.get("mesh"));
    let Some(mesh) = mesh else {
        return ExistingMesh::default();
    };
    let mesh_id = match mesh.get("mesh_id").and_then(|v| v.as_str()) {
        Some(s) => Value::String(s.to_string()),
        None => Value::Null,
    };
    ExistingMesh {
        mesh_id,
        carrier: mesh
            .get("carrier")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        channel: mesh.get("channel").and_then(norway_to_i64),
    }
}

/// Load the config as a YAML mapping, seeding an empty mapping when the file is
/// absent / unreadable / non-mapping (matching the Python `data: dict = {}` seed).
fn load_or_empty_mapping(config_path: &Path) -> serde_norway::Value {
    use serde_norway::Value as Yaml;
    match std::fs::read_to_string(config_path) {
        Ok(text) => match serde_norway::from_str::<Yaml>(&text) {
            Ok(v) if v.is_mapping() => v,
            _ => Yaml::Mapping(serde_norway::Mapping::new()),
        },
        Err(_) => Yaml::Mapping(serde_norway::Mapping::new()),
    }
}

/// Navigate/create a nested mapping path, replacing a non-mapping node along the
/// way with an empty mapping (the create-on-conflict behaviour the sibling config
/// merges use). Returns `None` only when the document root is not a mapping.
fn section_path_mut<'a>(
    data: &'a mut serde_norway::Value,
    path: &[&str],
) -> Option<&'a mut serde_norway::Mapping> {
    use serde_norway::Value as Yaml;
    let mut cur = data.as_mapping_mut()?;
    for key in path {
        let entry = cur
            .entry(Yaml::String((*key).to_string()))
            .or_insert_with(|| Yaml::Mapping(serde_norway::Mapping::new()));
        if !entry.is_mapping() {
            *entry = Yaml::Mapping(serde_norway::Mapping::new());
        }
        cur = entry.as_mapping_mut()?;
    }
    Some(cur)
}

/// Coerce a serde_norway scalar to `i64`, accepting an integer or a float (the
/// Python `int(...)` over a numeric config value). `None` for a non-number.
fn norway_to_i64(v: &serde_norway::Value) -> Option<i64> {
    match v {
        serde_norway::Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        _ => None,
    }
}

/// Serialize `data` to YAML and write it to `path` atomically (ensure the parent
/// dir, write a `.tmp` sibling, rename over the target). Returns the error string
/// on any serialize / I/O fault. Mirrors the tmp-write + `os.replace` idiom the
/// config persist uses.
fn write_atomic(path: &Path, data: &serde_norway::Value) -> Result<(), String> {
    let body = serde_norway::to_string(data).map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = {
        let mut ext = path
            .extension()
            .map(|e| e.to_os_string())
            .unwrap_or_default();
        ext.push(".tmp");
        path.with_extension(ext)
    };
    std::fs::write(&tmp, body.as_bytes()).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
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

/// The front's no-link 503 when the data-plane command socket is unreachable. The
/// FastAPI route runs the role transition in-process; the front cannot (it owns no
/// systemd lifecycle), so an absent socket degrades to a 503 rather than a 500 —
/// the same no-link posture the sibling write surfaces take on an absent seam.
fn socket_unavailable() -> Response {
    nested_detail(
        StatusCode::SERVICE_UNAVAILABLE,
        json!({
            "code": "E_GROUNDLINK_UNAVAILABLE",
            "message": "ground-station command socket unavailable",
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read a response body as JSON.
    async fn body_json(resp: Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    // ── profile gate ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn profile_mismatch_golden_body() {
        // The body every ground-station write returns on a drone, pinned as the
        // golden fixture for the conformance harness's off-a-drone diff.
        let resp = profile_mismatch();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            body_json(resp).await,
            json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})
        );
    }

    #[tokio::test]
    async fn not_in_mesh_body() {
        let resp = nested_detail(StatusCode::NOT_FOUND, json!({"code": "E_NOT_IN_MESH"}));
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            body_json(resp).await,
            json!({"detail": {"error": {"code": "E_NOT_IN_MESH"}}})
        );
    }

    // ── strip_ok ──────────────────────────────────────────────────────────────

    #[test]
    fn strip_ok_removes_the_transport_flag() {
        let reply = json!({
            "ok": true,
            "role": "relay",
            "previous": "direct",
            "units_started": ["ados-batman.service", "ados-wfb-relay.service"],
            "units_stopped": [],
            "ts_ms": 1234,
            "noop": false,
        });
        let body = strip_ok(reply).unwrap();
        assert!(!body.contains_key("ok"));
        assert_eq!(body.get("role"), Some(&json!("relay")));
        assert_eq!(body.get("previous"), Some(&json!("direct")));
        assert_eq!(body.get("noop"), Some(&json!(false)));
        assert_eq!(body.get("ts_ms"), Some(&json!(1234)));
    }

    #[test]
    fn strip_ok_surfaces_an_error_reply() {
        let err = strip_ok(json!({"ok": false, "error": "E_BATCTL_UNAVAILABLE"})).unwrap_err();
        assert_eq!(err.code, "E_BATCTL_UNAVAILABLE");
        assert!(err.message.is_none());
        let err2 = strip_ok(json!({"ok": false, "error": "E_INVALID_ROLE", "message": "bad"}))
            .unwrap_err();
        assert_eq!(err2.code, "E_INVALID_ROLE");
        assert_eq!(err2.message.as_deref(), Some("bad"));
    }

    // ── invalid-role 400 ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn socket_error_maps_to_a_nested_400() {
        let resp = socket_error_to_response(SocketError {
            code: "E_INVALID_ROLE".to_string(),
            message: Some(
                "role must be one of [\"direct\", \"relay\", \"receiver\"], got \"x\"".to_string(),
            ),
        });
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        assert_eq!(body["detail"]["error"]["code"], "E_INVALID_ROLE");
        assert!(body["detail"]["error"]["message"].as_str().is_some());
    }

    #[tokio::test]
    async fn socket_unavailable_is_a_503() {
        let resp = socket_unavailable();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body_json(resp).await;
        assert_eq!(body["detail"]["error"]["code"], "E_GROUNDLINK_UNAVAILABLE");
    }

    // ── mesh/config merge ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn mesh_config_merges_supplied_fields_and_keeps_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(
            &cfg,
            "agent:\n  name: gs-1\nground_station:\n  role: direct\n  mesh:\n    mesh_id: site-a\n    carrier: 802.11s\n    channel: 1\n",
        )
        .unwrap();

        let update = MeshConfigUpdate {
            mesh_id: Some("site-b".to_string()),
            carrier: Some("ibss".to_string()),
            channel: None,
        };
        let resp = put_mesh_config_at(&cfg, &update);
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        // mesh_id + carrier take the request; channel keeps the on-disk value.
        assert_eq!(
            body,
            json!({
                "mesh_id": "site-b",
                "carrier": "ibss",
                "channel": 1,
                "applied": true,
            })
        );

        // The on-disk merge applied the supplied fields, kept the unset one, and
        // preserved the unrelated keys.
        let parsed: serde_norway::Value =
            serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let mesh = parsed
            .get("ground_station")
            .and_then(|g| g.get("mesh"))
            .unwrap();
        assert_eq!(mesh.get("mesh_id").and_then(|v| v.as_str()), Some("site-b"));
        assert_eq!(mesh.get("carrier").and_then(|v| v.as_str()), Some("ibss"));
        assert_eq!(mesh.get("channel").and_then(norway_to_i64), Some(1));
        // The unrelated agent.name + the ground_station.role survived.
        assert_eq!(
            parsed
                .get("agent")
                .and_then(|a| a.get("name"))
                .and_then(|n| n.as_str()),
            Some("gs-1")
        );
        assert_eq!(
            parsed
                .get("ground_station")
                .and_then(|g| g.get("role"))
                .and_then(|r| r.as_str()),
            Some("direct")
        );
    }

    #[tokio::test]
    async fn mesh_config_empty_body_is_a_noop_with_defaults() {
        // An empty body (all fields null) is applied=false and echoes the existing
        // values (here: the config-model defaults, since the file is empty).
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let resp = put_mesh_config_at(&cfg, &MeshConfigUpdate::default());
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(
            body,
            json!({
                "mesh_id": Value::Null,
                "carrier": "802.11s",
                "channel": 1,
                "applied": false,
            })
        );
        // A no-op write leaves no file behind (the merge only writes when applied).
        assert!(!cfg.exists());
    }

    #[tokio::test]
    async fn mesh_config_no_existing_section_uses_model_defaults() {
        // A file with no ground_station.mesh section → only the supplied field is
        // written; the unset fields resolve to the config-model defaults.
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(&cfg, "agent:\n  name: x\n").unwrap();
        let update = MeshConfigUpdate {
            mesh_id: None,
            carrier: None,
            channel: Some(6),
        };
        let resp = put_mesh_config_at(&cfg, &update);
        let body = body_json(resp).await;
        assert_eq!(
            body,
            json!({
                "mesh_id": Value::Null,
                "carrier": "802.11s",
                "channel": 6,
                "applied": true,
            })
        );
        let parsed: serde_norway::Value =
            serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let mesh = parsed
            .get("ground_station")
            .and_then(|g| g.get("mesh"))
            .unwrap();
        assert_eq!(mesh.get("channel").and_then(norway_to_i64), Some(6));
        assert!(mesh.get("mesh_id").is_none());
        assert!(mesh.get("carrier").is_none());
    }

    // ── ground_station.role best-effort persist ───────────────────────────────

    #[test]
    fn merge_ground_station_role_persists_and_keeps_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(
            &cfg,
            "agent:\n  name: gs-1\nground_station:\n  role: direct\n",
        )
        .unwrap();
        merge_ground_station_role(&cfg, "relay").unwrap();
        let parsed: serde_norway::Value =
            serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(
            parsed
                .get("ground_station")
                .and_then(|g| g.get("role"))
                .and_then(|r| r.as_str()),
            Some("relay")
        );
        assert_eq!(
            parsed
                .get("agent")
                .and_then(|a| a.get("name"))
                .and_then(|n| n.as_str()),
            Some("gs-1")
        );
    }

    // ── existing_mesh projection ──────────────────────────────────────────────

    #[test]
    fn existing_mesh_reads_typed_fields_and_defaults_missing() {
        let data: serde_norway::Value = serde_norway::from_str(
            "ground_station:\n  mesh:\n    mesh_id: site-a\n    channel: 11\n",
        )
        .unwrap();
        let e = existing_mesh(&data);
        assert_eq!(e.mesh_id, json!("site-a"));
        assert_eq!(e.carrier, None);
        assert_eq!(e.channel, Some(11));
        // No section at all → null mesh_id + no carrier/channel.
        let empty: serde_norway::Value = serde_norway::from_str("agent:\n  name: x\n").unwrap();
        let e2 = existing_mesh(&empty);
        assert_eq!(e2.mesh_id, Value::Null);
        assert!(e2.carrier.is_none() && e2.channel.is_none());
    }
}
