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
//!   owns, and every role-owned unit (the supervisor's role table, the one the
//!   transition drives). Always 200 on a ground station.
//! - **`GET /api/v1/ground-station/mesh`** — a snapshot of batman-adv state. 404
//!   with `E_NOT_IN_MESH` on a `direct` node. Reads the durable store's most-recent
//!   `mesh.state` event first (the relay/receiver poll loop ships the same body it
//!   writes to the sidecar), falling back to the `/run/ados/mesh-state.json`
//!   sidecar when the store has nothing current. Both reads are age-gated to the
//!   `/status` snapshot window, so a dead poll loop reads as the empty object
//!   rather than as its last mesh; never a 500.
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

use ados_supervisor::config::VALID_ROLES;
use ados_supervisor::role::{role_units, ALL_ROLE_UNITS};

use crate::state::AppState;

// ---------------------------------------------------------------------------
// Profile + role gating (mirrors the FastAPI `_require_ground_profile` +
// `role_manager.get_current_role`).
// ---------------------------------------------------------------------------

/// Resolve the wire profile off explicit config + profile.conf + role-sentinel
/// paths. The native surface resolves the profile the same way the heartbeat does
/// (`crate::profile`), so a node installed with `profile: auto` that resolves to
/// `ground-station` passes the gate even though its raw config field is `"auto"`.
/// Threaded so a test drives the gate against a tempdir without mutating the
/// process environment. Mirrors the FastAPI `is_ground_station`.
fn resolved_profile_at(config: &Path, profile_conf: &Path, role_path: &Path) -> String {
    let config_profile = config_agent_profile_at(config);
    let (profile, _role) =
        crate::profile::current_profile_and_role_at(&config_profile, profile_conf, role_path);
    profile
}

/// True when the node's resolved profile is a ground station.
fn is_ground_station() -> bool {
    is_ground_station_at(
        &config_path(),
        &ados_config::profile_conf_path(),
        &crate::profile::mesh_role_path(),
    )
}

/// The path-injectable core of [`is_ground_station`], for tests.
fn is_ground_station_at(config: &Path, profile_conf: &Path, role_path: &Path) -> bool {
    resolved_profile_at(config, profile_conf, role_path) == "ground-station"
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

/// The raw `agent.profile` value from an explicit config path, defaulting to
/// `"auto"` when the section/field is absent — the same default the Python
/// `AgentConfig.profile` carries, so the profile resolver sees the identical input.
/// Threaded so a test drives it against a tempdir without touching the environment.
fn config_agent_profile_at(config: &Path) -> String {
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
    let cfg: ProfileConfig = std::fs::read_to_string(config)
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
        Self::load_from(&config_path())
    }

    /// The path-injectable core of [`load`], for tests: read the slice from an
    /// explicit config path without touching the process environment.
    fn load_from(config: &Path) -> Self {
        std::fs::read_to_string(config)
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
    current_role_at(&mesh_role_path())
}

/// The path-injectable core of [`current_role`]: read the role sentinel at an
/// explicit path. Threaded so a test drives it against a tempdir without mutating
/// the process environment.
fn current_role_at(role_path: &Path) -> String {
    if let Ok(text) = std::fs::read_to_string(role_path) {
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

/// Read a JSON sidecar into an object map, returning the empty object on any failure or a falsy /
/// non-object body. A falsy parse (null/false/0/""/[]/{}) and a non-object body both read as the
/// empty object, so the slice routes never index into a non-map.
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

/// How fresh a mesh snapshot must be to be served as the current mesh: the same
/// window the `/status` mesh block applies to the same `mesh.state` rows and
/// sidecar, so the two surfaces never disagree about a dead poll loop.
fn mesh_fresh_window() -> std::time::Duration {
    std::time::Duration::from_secs_f64(crate::routes::gs_status::SNAPSHOT_FRESH_S)
}

/// The current mesh-state body: the store's newest `mesh.state` event the
/// relay/receiver poll loop shipped (the same body written to `mesh-state.json`),
/// else the sidecar itself, each only when written inside [`mesh_fresh_window`].
/// The empty object when neither is current: the poll loop writes at ~1 Hz, so an
/// older snapshot is the last thing a dead loop wrote, not the mesh.
async fn current_mesh_snapshot(state: &AppState) -> Map<String, Value> {
    if let Some(detail) = state
        .logd
        .fresh_event_detail("mesh.state", mesh_fresh_window())
        .await
    {
        return detail;
    }
    fresh_mesh_sidecar(&mesh_state_path(), std::time::SystemTime::now())
}

/// The mesh-state sidecar when its mtime is inside [`mesh_fresh_window`] of
/// `now`, else the empty object.
fn fresh_mesh_sidecar(path: &Path, now: std::time::SystemTime) -> Map<String, Value> {
    if !crate::freshness::is_fresh(path, now, mesh_fresh_window()) {
        return Map::new();
    }
    read_json_object_or_empty(path)
}

/// Project the `/mesh/neighbors` shape from a snapshot body: `{"neighbors": ...}`,
/// defaulting to the empty list.
fn slice_neighbors(detail: &Map<String, Value>) -> Value {
    json!({ "neighbors": detail.get("neighbors").cloned().unwrap_or_else(|| json!([])) })
}

/// Project the `/mesh/routes` shape: `{"routes": <neighbors>}`. Routes are aliased to
/// neighbors on the live path today.
fn slice_routes(detail: &Map<String, Value>) -> Value {
    json!({ "routes": detail.get("neighbors").cloned().unwrap_or_else(|| json!([])) })
}

/// Project the `/mesh/gateways` shape: `{"gateways": ..., "selected": ...}`. The store
/// path keys `selected` off `selected_gateway`.
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
/// sentinel, `configured` from the agent config (default `direct`), the supported list,
/// the units the current role owns, and every role-owned unit.
pub async fn get_role() -> Response {
    use axum::response::IntoResponse;
    if !is_ground_station() {
        return profile_mismatch();
    }
    let cfg = MeshRouteConfig::load();
    let current = current_role();
    Json(role_body(&current, &cfg.ground_station.role)).into_response()
}

/// The role read's body for the sentinel's `current` role and the config's
/// `configured` one.
fn role_body(current: &str, configured: &str) -> Value {
    json!({
        "role": current,
        "configured": configured,
        "supported": VALID_ROLES,
        "units": role_units(current),
        "all_mesh_units": ALL_ROLE_UNITS,
    })
}

// ---------------------------------------------------------------------------
// GET /api/v1/ground-station/mesh
// ---------------------------------------------------------------------------

/// `GET /api/v1/ground-station/mesh` → the batman-adv state snapshot.
///
/// 404 with `E_PROFILE_MISMATCH` off a ground station, 404 with `E_NOT_IN_MESH` on a
/// `direct` node. Otherwise the store's most-recent `mesh.state` body, falling back to
/// the `mesh-state.json` sidecar (the empty object when neither is available).
pub async fn get_mesh_health(State(state): State<AppState>) -> Response {
    use axum::response::IntoResponse;
    if !is_ground_station() {
        return profile_mismatch();
    }
    if current_role() == "direct" {
        return not_in_mesh();
    }
    Json(Value::Object(current_mesh_snapshot(&state).await)).into_response()
}

// ---------------------------------------------------------------------------
// GET /api/v1/ground-station/mesh/neighbors
// ---------------------------------------------------------------------------

/// `GET /api/v1/ground-station/mesh/neighbors` → the `{neighbors}` slice. Same gates as
/// `/mesh`.
pub async fn get_mesh_neighbors(State(state): State<AppState>) -> Response {
    use axum::response::IntoResponse;
    if !is_ground_station() {
        return profile_mismatch();
    }
    if current_role() == "direct" {
        return not_in_mesh();
    }
    Json(slice_neighbors(&current_mesh_snapshot(&state).await)).into_response()
}

// ---------------------------------------------------------------------------
// GET /api/v1/ground-station/mesh/routes
// ---------------------------------------------------------------------------

/// `GET /api/v1/ground-station/mesh/routes` → the `{routes}` slice (aliased to
/// neighbors). Same gates as `/mesh`.
pub async fn get_mesh_routes(State(state): State<AppState>) -> Response {
    use axum::response::IntoResponse;
    if !is_ground_station() {
        return profile_mismatch();
    }
    if current_role() == "direct" {
        return not_in_mesh();
    }
    Json(slice_routes(&current_mesh_snapshot(&state).await)).into_response()
}

// ---------------------------------------------------------------------------
// GET /api/v1/ground-station/mesh/gateways
// ---------------------------------------------------------------------------

/// `GET /api/v1/ground-station/mesh/gateways` → the `{gateways, selected}` slice. Same
/// gates as `/mesh`.
pub async fn get_mesh_gateways(State(state): State<AppState>) -> Response {
    use axum::response::IntoResponse;
    if !is_ground_station() {
        return profile_mismatch();
    }
    if current_role() == "direct" {
        return not_in_mesh();
    }
    Json(slice_gateways(&current_mesh_snapshot(&state).await)).into_response()
}

// ---------------------------------------------------------------------------
// GET /api/v1/ground-station/mesh/config
// ---------------------------------------------------------------------------

/// `GET /api/v1/ground-station/mesh/config` → the configured mesh transport fields. 404
/// with `E_PROFILE_MISMATCH` off a ground station, otherwise always 200 with `{mesh_id,
/// carrier, channel, bat_iface, interface_override}` off the agent config (with the
/// config-model defaults when the section is absent).
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A tempdir wired with a config + role sentinel + the resolved seam paths, so
    /// the gating helpers read fixtures via the path-injectable cores. No process
    /// env is mutated, so the test cannot race a sibling test.
    struct Env {
        _dir: tempfile::TempDir,
        config: PathBuf,
        profile_conf: PathBuf,
        role_path: PathBuf,
    }

    fn with_env(role: Option<&str>, config_body: &str) -> Env {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.yaml");
        std::fs::write(&config, config_body).unwrap();
        // The profile resolver reads profile.conf only when the config field is
        // "auto"/empty; an explicit profile in config_body wins, so point the
        // sentinel at an absent path to keep the resolver deterministic.
        let profile_conf = dir.path().join("absent.conf");
        let role_path = dir.path().join("role");
        if let Some(r) = role {
            std::fs::write(&role_path, format!("{r}\n")).unwrap();
        }
        Env {
            _dir: dir,
            config,
            profile_conf,
            role_path,
        }
    }

    #[test]
    fn drone_profile_does_not_pass_the_gate() {
        let env = with_env(None, "agent:\n  profile: drone\n");
        assert!(!is_ground_station_at(
            &env.config,
            &env.profile_conf,
            &env.role_path
        ));
    }

    #[test]
    fn ground_station_profile_passes_the_gate() {
        let env = with_env(Some("direct"), "agent:\n  profile: ground_station\n");
        assert!(is_ground_station_at(
            &env.config,
            &env.profile_conf,
            &env.role_path
        ));
    }

    #[test]
    fn current_role_reads_the_sentinel_and_defaults_direct() {
        let env = with_env(Some("relay"), "agent:\n  profile: ground_station\n");
        assert_eq!(current_role_at(&env.role_path), "relay");
        // An absent sentinel defaults to direct.
        let absent = env.role_path.parent().unwrap().join("nonexistent-role");
        assert_eq!(current_role_at(&absent), "direct");
    }

    /// The golden role body the GCS reads on a relay-role and a direct-role
    /// ground station: the direct role owns the single-node receive plane.
    #[test]
    fn role_body_is_the_golden_shape() {
        let all = json!([
            "ados-wfb-rx.service",
            "ados-batman.service",
            "ados-wfb-relay.service",
            "ados-wfb-receiver.service",
        ]);
        assert_eq!(
            role_body("relay", "relay"),
            json!({
                "role": "relay",
                "configured": "relay",
                "supported": ["direct", "relay", "receiver"],
                "units": ["ados-batman.service", "ados-wfb-relay.service"],
                "all_mesh_units": all,
            })
        );
        assert_eq!(
            role_body("direct", "direct")["units"],
            json!(["ados-wfb-rx.service"])
        );
    }

    #[test]
    fn role_config_default_is_direct_when_section_absent() {
        let env = with_env(Some("direct"), "agent:\n  profile: ground_station\n");
        let cfg = MeshRouteConfig::load_from(&env.config);
        assert_eq!(cfg.ground_station.role, "direct");
    }

    #[test]
    fn mesh_config_defaults_match_the_model_when_the_section_is_absent() {
        // A config with no ground_station section reads the MeshConfig defaults.
        let env = with_env(Some("direct"), "agent:\n  profile: ground_station\n");
        let mesh = MeshRouteConfig::load_from(&env.config).ground_station.mesh;
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
        let env = with_env(Some("direct"), body);
        let mesh = MeshRouteConfig::load_from(&env.config).ground_station.mesh;
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
    fn a_mesh_sidecar_a_dead_poll_loop_left_behind_reads_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mesh-state.json");
        std::fs::write(&path, r#"{"up":true,"neighbors":[{"id":"a"}]}"#).unwrap();
        let written = std::fs::metadata(&path).unwrap().modified().unwrap();
        let live = fresh_mesh_sidecar(&path, written + std::time::Duration::from_secs(1));
        assert_eq!(live["up"], json!(true));
        let dead = fresh_mesh_sidecar(&path, written + mesh_fresh_window() * 2);
        assert_eq!(dead, Map::new());
        assert_eq!(slice_neighbors(&dead), json!({"neighbors": []}));
    }

    #[test]
    fn error_responses_are_404() {
        assert_eq!(profile_mismatch().status(), StatusCode::NOT_FOUND);
        assert_eq!(not_in_mesh().status(), StatusCode::NOT_FOUND);
    }
}
