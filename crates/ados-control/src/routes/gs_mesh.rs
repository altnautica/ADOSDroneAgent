//! Ground-station mesh role + state read routes.
//!
//! Six read-only routes the GCS Hardware tab polls on a ground-station node. All
//! are profile-gated: a node whose resolved profile is not `ground-station` gets
//! a 404 with the `E_PROFILE_MISMATCH` code (the same body the FastAPI gate
//! raises). The mesh-state reads add a second gate: a node in the `direct` role is
//! not in a mesh, so they 404 with `E_NOT_IN_MESH`.
//!
//! - **`GET /api/v1/ground-station/role`** — the current mesh role (read from the
//!   on-disk role sentinel, defaulting to `direct`), the role configured in the
//!   agent config, the supported-role list, the systemd units the current role
//!   owns, and the full mesh-unit set. Always 200 on a ground station.
//! - **`GET /api/v1/ground-station/mesh`** — a snapshot of batman-adv state. 404
//!   with `E_NOT_IN_MESH` on a `direct` node. Reads the durable store's most-recent
//!   `mesh.state` event first (the relay/receiver poll loop ships the same body it
//!   writes to the sidecar), falling back to the `/run/ados/mesh-state.json`
//!   sidecar when the store is unreachable, so a losable store degrades to the old
//!   behavior, never to a 500.
//! - **`GET /api/v1/ground-station/mesh/neighbors`** — the `{neighbors}` slice of
//!   that snapshot.
//! - **`GET /api/v1/ground-station/mesh/routes`** — the `{routes}` slice (routes
//!   are aliased to neighbors on the live path today).
//! - **`GET /api/v1/ground-station/mesh/gateways`** — the `{gateways, selected}`
//!   slice.
//! - **`GET /api/v1/ground-station/mesh/config`** — the configured mesh transport
//!   fields (`mesh_id`, `carrier`, `channel`, `bat_iface`, `interface_override`)
//!   off the agent config, with the same defaults the config model carries when
//!   the section is absent. Always 200 on a ground station.
//!
//! The websocket routes (`/ws/uplink`, `/ws/mesh`) and the write routes
//! (`PUT /role`, `PUT /mesh/gateway_preference`, `PUT /mesh/config`) stay on the
//! residual surface; only the exact-path GET reads move here.

use std::path::{Path, PathBuf};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::state::AppState;

// ---------------------------------------------------------------------------
// Profile + role gating (mirrors the FastAPI `_require_ground_profile` +
// `role_manager.get_current_role`).
// ---------------------------------------------------------------------------

/// The valid mesh roles, in advertised order. Mirrors the Python
/// `role_manager.VALID_ROLES`.
const VALID_ROLES: [&str; 3] = ["direct", "relay", "receiver"];

/// The systemd units each role owns, in start order. Mirrors the Python
/// `role_manager._ROLE_UNITS` (direct owns none; relay and receiver both bring up
/// `ados-batman` before their wfb unit). The order is load-bearing for the `units`
/// field the role route returns.
fn role_units(role: &str) -> Vec<&'static str> {
    match role {
        "relay" => vec!["ados-batman.service", "ados-wfb-relay.service"],
        "receiver" => vec!["ados-batman.service", "ados-wfb-receiver.service"],
        // direct (and any unknown value) owns no mesh units.
        _ => vec![],
    }
}

/// The full mesh-unit set, in the Python `role_manager._ALL_MESH_UNITS` order.
fn all_mesh_units() -> Vec<&'static str> {
    vec![
        "ados-batman.service",
        "ados-wfb-relay.service",
        "ados-wfb-receiver.service",
    ]
}

/// The resolved profile for the gate. The native surface resolves the profile the
/// same way the heartbeat does (`crate::profile`), so a node installed with
/// `profile: auto` that resolves to `ground-station` passes the gate even though
/// its raw config field is `"auto"`. Mirrors the FastAPI `is_ground_station`.
fn resolved_profile() -> String {
    let config_profile = config_agent_profile();
    let (profile, _role) = crate::profile::current_profile_and_role(&config_profile);
    profile
}

/// True when the node's resolved profile is a ground station.
fn is_ground_station() -> bool {
    resolved_profile() == "ground-station"
}

/// The FastAPI profile-mismatch error: a 404 whose `detail` is the nested
/// `{"error": {"code": "E_PROFILE_MISMATCH"}}` object the gate raises (NOT a plain
/// string). Built directly here because the shared `crate::routes::detail` helper
/// emits a string `detail`, which would not match this route's nested shape.
fn profile_mismatch() -> Response {
    nested_detail(StatusCode::NOT_FOUND, json!({"code": "E_PROFILE_MISMATCH"}))
}

/// The FastAPI not-in-mesh error: a 404 whose `detail` is the nested
/// `{"error": {"code": "E_NOT_IN_MESH"}}` object the mesh reads raise on a `direct`
/// node.
fn not_in_mesh() -> Response {
    nested_detail(StatusCode::NOT_FOUND, json!({"code": "E_NOT_IN_MESH"}))
}

/// Build a `(status, {"detail": {"error": <error>}})` response. FastAPI wraps an
/// `HTTPException(detail={"error": {...}})` as `{"detail": {"error": {...}}}`, so
/// the native body matches byte-for-byte.
fn nested_detail(status: StatusCode, error: Value) -> Response {
    use axum::response::IntoResponse;
    (status, Json(json!({ "detail": { "error": error } }))).into_response()
}

// ---------------------------------------------------------------------------
// Config seam: the agent config the role + mesh-config routes read.
// ---------------------------------------------------------------------------

/// The config path (`ADOS_CONFIG`, default `/etc/ados/config.yaml`), the same seam
/// the wave-1 status/wfb routes resolve under.
fn config_path() -> PathBuf {
    let raw =
        std::env::var("ADOS_CONFIG").unwrap_or_else(|_| crate::config::CONFIG_YAML.to_string());
    PathBuf::from(raw)
}

/// The raw `agent.profile` value from the config, defaulting to `"auto"` when the
/// section/field is absent — the same default the Python `AgentConfig.profile`
/// carries, so the profile resolver sees the identical input.
fn config_agent_profile() -> String {
    #[derive(Debug, Clone, Deserialize)]
    struct AgentSection {
        #[serde(default = "default_profile")]
        profile: String,
    }
    fn default_profile() -> String {
        "auto".to_string()
    }
    #[derive(Debug, Clone, Default, Deserialize)]
    struct ProfileConfig {
        #[serde(default)]
        agent: Option<AgentSection>,
    }
    let cfg: ProfileConfig = std::fs::read_to_string(config_path())
        .ok()
        .and_then(|text| serde_norway::from_str(&text).ok())
        .unwrap_or_default();
    cfg.agent.map(|a| a.profile).unwrap_or_else(default_profile)
}

/// The `ground_station.mesh` slice the mesh-config route projects. Each field
/// carries the exact default the Python `MeshConfig` model uses, so an absent
/// section reads byte-identically (`mesh_id: null`, `carrier: "802.11s"`,
/// `channel: 1`, `bat_iface: "bat0"`, `interface_override: null`).
#[derive(Debug, Clone, Deserialize)]
struct MeshConfigSection {
    #[serde(default)]
    interface_override: Option<String>,
    #[serde(default = "default_carrier")]
    carrier: String,
    #[serde(default)]
    mesh_id: Option<String>,
    #[serde(default = "default_channel")]
    channel: i64,
    #[serde(default = "default_bat_iface")]
    bat_iface: String,
}

fn default_carrier() -> String {
    "802.11s".to_string()
}

fn default_channel() -> i64 {
    1
}

fn default_bat_iface() -> String {
    "bat0".to_string()
}

impl Default for MeshConfigSection {
    fn default() -> Self {
        MeshConfigSection {
            interface_override: None,
            carrier: default_carrier(),
            mesh_id: None,
            channel: default_channel(),
            bat_iface: default_bat_iface(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct GroundStationSection {
    #[serde(default)]
    mesh: MeshConfigSection,
    #[serde(default = "default_gs_role")]
    role: String,
}

fn default_gs_role() -> String {
    "direct".to_string()
}

impl Default for GroundStationSection {
    fn default() -> Self {
        // Mirror the Python `GroundStationConfig` defaults: an absent section still
        // reads `role: "direct"` (the role route's `configured` default), not an
        // empty string. The `#[derive(Default)]` for `String` would give `""`.
        GroundStationSection {
            mesh: MeshConfigSection::default(),
            role: default_gs_role(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct MeshRouteConfig {
    #[serde(default)]
    ground_station: GroundStationSection,
}

impl MeshRouteConfig {
    /// Load the `ground_station` slice from the config path. A missing or
    /// unparseable file yields the all-defaults slice, so both the role and
    /// mesh-config routes still answer a usable body.
    fn load() -> Self {
        std::fs::read_to_string(config_path())
            .ok()
            .and_then(|text| serde_norway::from_str(&text).ok())
            .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Role sentinel seam: the on-disk role file the role route reads.
// ---------------------------------------------------------------------------

/// The ground-station role sentinel (`/etc/ados/mesh/role`), overridable via
/// `ADOS_MESH_ROLE` for tests — the same override `crate::profile` resolves under.
fn mesh_role_path() -> PathBuf {
    std::env::var("ADOS_MESH_ROLE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(crate::profile::MESH_ROLE_PATH))
}

/// Read the on-disk role sentinel, defaulting to `direct` when the file is
/// missing, unreadable, or carries an unknown value. Mirrors the Python
/// `role_manager.get_current_role`.
fn current_role() -> String {
    if let Ok(text) = std::fs::read_to_string(mesh_role_path()) {
        let value = text.trim();
        if VALID_ROLES.contains(&value) {
            return value.to_string();
        }
    }
    "direct".to_string()
}

// ---------------------------------------------------------------------------
// Mesh-state seam: the sidecar file + the durable store's `mesh.state` event.
// ---------------------------------------------------------------------------

/// The runtime dir (`ADOS_RUN_DIR`, default `/run/ados`), the same override the
/// sibling sockets + sentinels resolve under.
fn run_dir() -> PathBuf {
    PathBuf::from(std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string()))
}

/// The live mesh-state sidecar (`/run/ados/mesh-state.json`), written by the
/// native mesh poll loop.
fn mesh_state_path() -> PathBuf {
    run_dir().join("mesh-state.json")
}

/// Read a JSON sidecar into an object map, returning the empty object on any
/// failure or a falsy / non-object body. Mirrors the Python
/// `_read_json_or_empty`, which returns `json.loads(text) or {}` (a falsy parse
/// — null/false/0/""/[]/{} — collapses to `{}`); a non-object truthy body is not a
/// realistic mesh sidecar, so it also reads as the empty object here rather than
/// risking a `.get` failure on the slice routes (strictly safer than the Python
/// path, and byte-identical for every real dict/empty input).
fn read_json_object_or_empty(path: &Path) -> Map<String, Value> {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(map)) => {
                // This helper reads only the `mesh-state.json` sidecar in this
                // module. Best-effort schema-drift signal (never reject): warn on a
                // producer/reader version mismatch, then read anyway. The writer
                // const lives in the groundlink crate, so compare against the
                // shared registry.
                let got = map.get("version").and_then(Value::as_u64).unwrap_or(0) as u16;
                if let Some(ours) = ados_protocol::contracts::sidecar_version("mesh-state") {
                    ados_protocol::sidecar::check_sidecar_version("mesh-state", got, ours);
                }
                map
            }
            _ => Map::new(),
        },
        Err(_) => Map::new(),
    }
}

/// The most-recent full mesh-state body from the durable store, or `None`.
///
/// Queries the store for the newest `mesh.state` event the relay/receiver poll
/// loop shipped and returns its non-empty `detail` map (the same body written to
/// `mesh-state.json`). `None` when the store is unreachable, holds no such event,
/// or the `detail` is absent / non-object / empty, so the caller falls back to the
/// sidecar file. Mirrors the Python `latest_mesh_snapshot`.
async fn latest_mesh_snapshot(state: &AppState) -> Option<Map<String, Value>> {
    let rows = logd_query_events(state, "mesh.state", 1).await?;
    let row = rows.first()?.as_object()?;
    let detail = row.get("detail")?.as_object()?;
    if detail.is_empty() {
        return None;
    }
    Some(detail.clone())
}

/// Project the `/mesh/neighbors` shape from a snapshot body: `{"neighbors": ...}`,
/// defaulting to the empty list. Mirrors the Python `slice_neighbors` /
/// `{"neighbors": snap.get("neighbors", [])}`.
fn slice_neighbors(detail: &Map<String, Value>) -> Value {
    json!({ "neighbors": detail.get("neighbors").cloned().unwrap_or_else(|| json!([])) })
}

/// Project the `/mesh/routes` shape: `{"routes": <neighbors>}`. Routes are aliased
/// to neighbors on the live path today. Mirrors the Python `slice_routes` /
/// `{"routes": snap.get("neighbors", [])}`.
fn slice_routes(detail: &Map<String, Value>) -> Value {
    json!({ "routes": detail.get("neighbors").cloned().unwrap_or_else(|| json!([])) })
}

/// Project the `/mesh/gateways` shape: `{"gateways": ..., "selected": ...}`. The
/// store path keys `selected` off `selected_gateway`. Mirrors the Python
/// `slice_gateways` / `{"gateways": snap.get("gateways", []), "selected":
/// snap.get("selected_gateway")}`.
fn slice_gateways(detail: &Map<String, Value>) -> Value {
    json!({
        "gateways": detail.get("gateways").cloned().unwrap_or_else(|| json!([])),
        "selected": detail.get("selected_gateway").cloned().unwrap_or(Value::Null),
    })
}

// ---------------------------------------------------------------------------
// GET /api/v1/ground-station/role
// ---------------------------------------------------------------------------

/// `GET /api/v1/ground-station/role` → the current mesh role + capability hint.
///
/// 404 with `E_PROFILE_MISMATCH` off a ground station. Otherwise always 200 with
/// `{role, configured, supported, units, all_mesh_units}`: `role` from the on-disk
/// sentinel, `configured` from the agent config (default `direct`), the supported
/// list, the units the current role owns, and the full mesh-unit set. Mirrors the
/// Python `get_role`.
pub async fn get_role() -> Response {
    use axum::response::IntoResponse;
    if !is_ground_station() {
        return profile_mismatch();
    }
    let cfg = MeshRouteConfig::load();
    let current = current_role();
    Json(json!({
        "role": current,
        "configured": cfg.ground_station.role,
        "supported": VALID_ROLES,
        "units": role_units(&current),
        "all_mesh_units": all_mesh_units(),
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// GET /api/v1/ground-station/mesh
// ---------------------------------------------------------------------------

/// `GET /api/v1/ground-station/mesh` → the batman-adv state snapshot.
///
/// 404 with `E_PROFILE_MISMATCH` off a ground station, 404 with `E_NOT_IN_MESH` on
/// a `direct` node. Otherwise the store's most-recent `mesh.state` body, falling
/// back to the `mesh-state.json` sidecar (the empty object when neither is
/// available). Mirrors the Python `get_mesh_health`.
pub async fn get_mesh_health(State(state): State<AppState>) -> Response {
    use axum::response::IntoResponse;
    if !is_ground_station() {
        return profile_mismatch();
    }
    if current_role() == "direct" {
        return not_in_mesh();
    }
    if let Some(detail) = latest_mesh_snapshot(&state).await {
        return Json(Value::Object(detail)).into_response();
    }
    Json(Value::Object(read_json_object_or_empty(&mesh_state_path()))).into_response()
}

// ---------------------------------------------------------------------------
// GET /api/v1/ground-station/mesh/neighbors
// ---------------------------------------------------------------------------

/// `GET /api/v1/ground-station/mesh/neighbors` → the `{neighbors}` slice. Same
/// gates as `/mesh`. Mirrors the Python `get_mesh_neighbors`.
pub async fn get_mesh_neighbors(State(state): State<AppState>) -> Response {
    use axum::response::IntoResponse;
    if !is_ground_station() {
        return profile_mismatch();
    }
    if current_role() == "direct" {
        return not_in_mesh();
    }
    if let Some(detail) = latest_mesh_snapshot(&state).await {
        return Json(slice_neighbors(&detail)).into_response();
    }
    let snap = read_json_object_or_empty(&mesh_state_path());
    Json(slice_neighbors(&snap)).into_response()
}

// ---------------------------------------------------------------------------
// GET /api/v1/ground-station/mesh/routes
// ---------------------------------------------------------------------------

/// `GET /api/v1/ground-station/mesh/routes` → the `{routes}` slice (aliased to
/// neighbors). Same gates as `/mesh`. Mirrors the Python `get_mesh_routes`.
pub async fn get_mesh_routes(State(state): State<AppState>) -> Response {
    use axum::response::IntoResponse;
    if !is_ground_station() {
        return profile_mismatch();
    }
    if current_role() == "direct" {
        return not_in_mesh();
    }
    if let Some(detail) = latest_mesh_snapshot(&state).await {
        return Json(slice_routes(&detail)).into_response();
    }
    let snap = read_json_object_or_empty(&mesh_state_path());
    Json(slice_routes(&snap)).into_response()
}

// ---------------------------------------------------------------------------
// GET /api/v1/ground-station/mesh/gateways
// ---------------------------------------------------------------------------

/// `GET /api/v1/ground-station/mesh/gateways` → the `{gateways, selected}` slice.
/// Same gates as `/mesh`. Mirrors the Python `get_mesh_gateways`.
pub async fn get_mesh_gateways(State(state): State<AppState>) -> Response {
    use axum::response::IntoResponse;
    if !is_ground_station() {
        return profile_mismatch();
    }
    if current_role() == "direct" {
        return not_in_mesh();
    }
    if let Some(detail) = latest_mesh_snapshot(&state).await {
        return Json(slice_gateways(&detail)).into_response();
    }
    let snap = read_json_object_or_empty(&mesh_state_path());
    Json(slice_gateways(&snap)).into_response()
}

// ---------------------------------------------------------------------------
// GET /api/v1/ground-station/mesh/config
// ---------------------------------------------------------------------------

/// `GET /api/v1/ground-station/mesh/config` → the configured mesh transport
/// fields. 404 with `E_PROFILE_MISMATCH` off a ground station, otherwise always
/// 200 with `{mesh_id, carrier, channel, bat_iface, interface_override}` off the
/// agent config (with the config-model defaults when the section is absent).
/// Mirrors the Python `get_mesh_config`.
pub async fn get_mesh_config() -> Response {
    use axum::response::IntoResponse;
    if !is_ground_station() {
        return profile_mismatch();
    }
    let mesh = MeshRouteConfig::load().ground_station.mesh;
    Json(json!({
        "mesh_id": mesh.mesh_id,
        "carrier": mesh.carrier,
        "channel": mesh.channel,
        "bat_iface": mesh.bat_iface,
        "interface_override": mesh.interface_override,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// logd query seam: HTTP-over-UDS reads of the store's /v1 API.
// ---------------------------------------------------------------------------

/// Query the store for the newest `events` rows of one `event_kind`. Returns the
/// `data` array, or `None` when the store is unreachable / the response is an
/// error / does not parse. Mirrors the Python `query_rows("events", limit,
/// event_kind=...)`.
async fn logd_query_events(state: &AppState, event_kind: &str, limit: i64) -> Option<Vec<Value>> {
    let params = [
        ("kind", "events".to_string()),
        ("limit", limit.to_string()),
        ("event_kind", event_kind.to_string()),
    ];
    let query = encode_query(&params);
    let path = format!("/v1/query?{query}");
    let (status, body) = logd_get(state, &path).await.ok()?;
    if status >= 400 {
        return None;
    }
    let parsed: Value = serde_json::from_slice(&body).ok()?;
    parsed
        .get("data")
        .and_then(Value::as_array)
        .map(|a| a.to_vec())
}

/// A minimal HTTP/1.1 `GET` over the logging-store query Unix socket, returning the
/// status code + the decoded body. The socket path comes from the app state's logd
/// client so a test redirects it. `Connection: close` reads the body to EOF; a
/// chunked body is de-chunked. Bounded so a runaway response cannot exhaust memory.
async fn logd_get(state: &AppState, path: &str) -> std::io::Result<(u16, Vec<u8>)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A hard ceiling on the response read; a normal events page is a few KiB, so
    /// this only guards a runaway body.
    const MAX_READ_BYTES: usize = 4 * 1024 * 1024;

    let socket = state.logd.socket_path();
    let mut stream = tokio::net::UnixStream::connect(socket).await?;
    let head = format!("GET {path} HTTP/1.1\r\nHost: logd\r\nConnection: close\r\n\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.flush().await?;

    let mut raw = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            break; // EOF (Connection: close).
        }
        if raw.len() + n > MAX_READ_BYTES {
            return Err(std::io::Error::other("logd response too large"));
        }
        raw.extend_from_slice(&buf[..n]);
    }
    parse_http_response(&raw)
}

/// Split a raw HTTP/1.1 response into the status code + decoded body. De-chunks a
/// `Transfer-Encoding: chunked` body; otherwise returns the body after the header
/// terminator as-is.
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

/// De-chunk a `Transfer-Encoding: chunked` body: `<hexlen>\r\n<data>\r\n` repeated
/// until a zero-length chunk.
fn de_chunk(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(crlf) = rest.windows(2).position(|w| w == b"\r\n") {
        let len_line = &rest[..crlf];
        let len = usize::from_str_radix(String::from_utf8_lossy(len_line).trim(), 16).unwrap_or(0);
        if len == 0 {
            break;
        }
        let data_start = crlf + 2;
        if rest.len() < data_start + len {
            out.extend_from_slice(&rest[data_start..]);
            break;
        }
        out.extend_from_slice(&rest[data_start..data_start + len]);
        let next = data_start + len;
        rest = if rest.len() >= next + 2 {
            &rest[next + 2..]
        } else {
            &[]
        };
    }
    out
}

/// Percent-encode a query-parameter list into a `key=value&...` string.
fn encode_query(params: &[(&str, String)]) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Conservative percent-encoding: pass through the unreserved set
/// (`A-Za-z0-9-._~`) verbatim and percent-encode every other byte.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Point the config + role + run-dir seams at a tempdir so the gating helpers
    /// read fixtures, not the live host. Holds the env lock + the tempdir for the
    /// test's lifetime; dropping it clears the env vars.
    struct Env {
        _dir: tempfile::TempDir,
        _guard: tokio::sync::MutexGuard<'static, ()>,
    }

    impl Drop for Env {
        fn drop(&mut self) {
            std::env::remove_var("ADOS_CONFIG");
            std::env::remove_var("ADOS_PROFILE_CONF");
            std::env::remove_var("ADOS_MESH_ROLE");
            std::env::remove_var("ADOS_RUN_DIR");
        }
    }

    fn with_env(role: Option<&str>, config_body: &str) -> Env {
        // The crate-wide env lock, recovered even if a prior test panicked.
        let guard = crate::lock_env_blocking();
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(&cfg, config_body).unwrap();
        std::env::set_var("ADOS_CONFIG", &cfg);
        // The profile resolver reads profile.conf only when the config field is
        // "auto"/empty; an explicit profile in config_body wins, so point the
        // sentinel at an absent path to keep the resolver deterministic.
        std::env::set_var("ADOS_PROFILE_CONF", dir.path().join("absent.conf"));
        let role_path = dir.path().join("role");
        if let Some(r) = role {
            std::fs::write(&role_path, format!("{r}\n")).unwrap();
        }
        std::env::set_var("ADOS_MESH_ROLE", &role_path);
        std::env::set_var("ADOS_RUN_DIR", dir.path());
        Env {
            _dir: dir,
            _guard: guard,
        }
    }

    #[test]
    fn role_units_match_the_python_mapping() {
        assert_eq!(role_units("direct"), Vec::<&str>::new());
        assert_eq!(
            role_units("relay"),
            vec!["ados-batman.service", "ados-wfb-relay.service"]
        );
        assert_eq!(
            role_units("receiver"),
            vec!["ados-batman.service", "ados-wfb-receiver.service"]
        );
        // An unknown role owns no units (matches the dict `.get(role, [])`).
        assert_eq!(role_units("bogus"), Vec::<&str>::new());
    }

    #[test]
    fn all_mesh_units_is_the_full_set_in_order() {
        assert_eq!(
            all_mesh_units(),
            vec![
                "ados-batman.service",
                "ados-wfb-relay.service",
                "ados-wfb-receiver.service",
            ]
        );
    }

    #[test]
    fn drone_profile_does_not_pass_the_gate() {
        let _env = with_env(None, "agent:\n  profile: drone\n");
        assert!(!is_ground_station());
    }

    #[test]
    fn ground_station_profile_passes_the_gate() {
        let _env = with_env(Some("direct"), "agent:\n  profile: ground_station\n");
        assert!(is_ground_station());
    }

    #[test]
    fn current_role_reads_the_sentinel_and_defaults_direct() {
        let _env = with_env(Some("relay"), "agent:\n  profile: ground_station\n");
        assert_eq!(current_role(), "relay");
        // An absent sentinel defaults to direct.
        std::env::set_var("ADOS_MESH_ROLE", "/nonexistent/role");
        assert_eq!(current_role(), "direct");
    }

    /// The golden role body the GCS reads on a relay-role ground station with an
    /// explicit relay config role.
    #[test]
    fn role_body_is_the_golden_shape_on_a_relay_node() {
        let configured_role = "relay";
        let current = "relay";
        let body = json!({
            "role": current,
            "configured": configured_role,
            "supported": VALID_ROLES,
            "units": role_units(current),
            "all_mesh_units": all_mesh_units(),
        });
        let want = json!({
            "role": "relay",
            "configured": "relay",
            "supported": ["direct", "relay", "receiver"],
            "units": ["ados-batman.service", "ados-wfb-relay.service"],
            "all_mesh_units": [
                "ados-batman.service",
                "ados-wfb-relay.service",
                "ados-wfb-receiver.service",
            ],
        });
        assert_eq!(body, want);
    }

    #[test]
    fn role_config_default_is_direct_when_section_absent() {
        let _env = with_env(Some("direct"), "agent:\n  profile: ground_station\n");
        let cfg = MeshRouteConfig::load();
        assert_eq!(cfg.ground_station.role, "direct");
    }

    #[test]
    fn mesh_config_defaults_match_the_model_when_the_section_is_absent() {
        // A config with no ground_station section reads the MeshConfig defaults.
        let _env = with_env(Some("direct"), "agent:\n  profile: ground_station\n");
        let mesh = MeshRouteConfig::load().ground_station.mesh;
        let body = json!({
            "mesh_id": mesh.mesh_id,
            "carrier": mesh.carrier,
            "channel": mesh.channel,
            "bat_iface": mesh.bat_iface,
            "interface_override": mesh.interface_override,
        });
        let want = json!({
            "mesh_id": null,
            "carrier": "802.11s",
            "channel": 1,
            "bat_iface": "bat0",
            "interface_override": null,
        });
        assert_eq!(body, want);
    }

    #[test]
    fn mesh_config_reads_the_configured_values() {
        let body = "ground_station:\n  mesh:\n    mesh_id: site-a\n    carrier: ibss\n    channel: 6\n    bat_iface: bat1\n    interface_override: wlan2\n";
        let _env = with_env(Some("direct"), body);
        let mesh = MeshRouteConfig::load().ground_station.mesh;
        let got = json!({
            "mesh_id": mesh.mesh_id,
            "carrier": mesh.carrier,
            "channel": mesh.channel,
            "bat_iface": mesh.bat_iface,
            "interface_override": mesh.interface_override,
        });
        assert_eq!(
            got,
            json!({
                "mesh_id": "site-a",
                "carrier": "ibss",
                "channel": 6,
                "bat_iface": "bat1",
                "interface_override": "wlan2",
            })
        );
    }

    #[test]
    fn slicers_default_to_empty_collections() {
        // An empty snapshot yields the empty-list / null defaults.
        let empty = Map::new();
        assert_eq!(slice_neighbors(&empty), json!({"neighbors": []}));
        assert_eq!(slice_routes(&empty), json!({"routes": []}));
        assert_eq!(
            slice_gateways(&empty),
            json!({"gateways": [], "selected": null})
        );
    }

    #[test]
    fn slicers_project_a_populated_snapshot() {
        // The neighbors list is shared by /mesh/neighbors and /mesh/routes.
        let mut snap = Map::new();
        snap.insert("neighbors".to_string(), json!([{"mac": "aa:bb"}]));
        snap.insert("gateways".to_string(), json!([{"mac": "cc:dd"}]));
        snap.insert("selected_gateway".to_string(), json!("cc:dd"));
        assert_eq!(
            slice_neighbors(&snap),
            json!({"neighbors": [{"mac": "aa:bb"}]})
        );
        assert_eq!(slice_routes(&snap), json!({"routes": [{"mac": "aa:bb"}]}));
        assert_eq!(
            slice_gateways(&snap),
            json!({"gateways": [{"mac": "cc:dd"}], "selected": "cc:dd"})
        );
    }

    #[test]
    fn read_json_object_or_empty_handles_absent_and_non_object() {
        let dir = tempfile::tempdir().unwrap();
        // Absent file → empty object.
        assert_eq!(
            read_json_object_or_empty(&dir.path().join("absent.json")),
            Map::new()
        );
        // A non-object body (a JSON list) → empty object.
        let list = dir.path().join("list.json");
        std::fs::write(&list, "[1,2,3]").unwrap();
        assert_eq!(read_json_object_or_empty(&list), Map::new());
        // A real object body round-trips.
        let obj = dir.path().join("obj.json");
        std::fs::write(&obj, r#"{"neighbors":[],"gateways":[]}"#).unwrap();
        let got = read_json_object_or_empty(&obj);
        assert!(got.contains_key("neighbors"));
        assert!(got.contains_key("gateways"));
    }

    #[test]
    fn nested_error_detail_shapes_match_the_fastapi_codes() {
        let mismatch = json!({ "detail": { "error": { "code": "E_PROFILE_MISMATCH" } } });
        assert_eq!(
            mismatch["detail"]["error"]["code"],
            json!("E_PROFILE_MISMATCH")
        );
        let not_in = json!({ "detail": { "error": { "code": "E_NOT_IN_MESH" } } });
        assert_eq!(not_in["detail"]["error"]["code"], json!("E_NOT_IN_MESH"));
    }

    #[test]
    fn error_responses_are_404() {
        assert_eq!(profile_mismatch().status(), StatusCode::NOT_FOUND);
        assert_eq!(not_in_mesh().status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn de_chunk_reassembles_a_chunked_body() {
        let chunked = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        assert_eq!(de_chunk(chunked), b"hello world");
    }

    #[test]
    fn parse_http_response_reads_status_and_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
        let (status, body) = parse_http_response(raw).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"{}");
    }
}
