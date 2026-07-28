//! Single flight-controller parameter read route.
//!
//! `GET /api/params/{name}` returns one cached FC parameter by name. It reads the
//! same source the full parameter list ([`crate::routes::params`]) reads — the
//! MAVLink router's on-disk parameter cache ([`crate::param_store`]) — and filters
//! it to the one requested name. The map used to ride the vehicle-state IPC
//! snapshot as a `params` blob; it left that snapshot because republishing ~24 KB
//! of parameters ten times a second is what made a relayed read need ~21 aux-lane
//! fragments. The native front sits in front of the standalone API process, which
//! holds no in-process parameter cache or vehicle-state object, so the file the
//! router persists atomically is the only production-reachable source.
//!
//! When the name is present the body is `{"name": <name>, "value": <value>}` with
//! the value passed through verbatim from the cache (preserving its exact JSON
//! number form, the same way the full-list route clones the blob). When the name
//! is absent — an empty or unreadable cache, or a name not in it — the route
//! returns the FastAPI 404 `{"detail": "Parameter '<name>' not found"}`, the exact
//! status and message the proxied FastAPI route raised.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::routes::detail;
use crate::state::AppState;

/// `GET /api/params/{name}` → `{"name", "value"}` for a cached FC parameter, or a
/// 404 `{"detail"}` when the name is not in the cache.
///
/// Reads the value from the router's on-disk parameter cache (the same source the
/// full-list read uses). A name present in the cache returns `200` with the value
/// verbatim; any absent-file / unreadable / missing-name case returns the FastAPI
/// `404` with the byte-identical not-found message. Never panics on a seam error:
/// an absent cache is the not-found case, never a 500.
pub async fn get_param(Path(name): Path<String>, State(state): State<AppState>) -> Response {
    let params = crate::param_store::read_param_blob(&state.params_path);
    match params.get(&name) {
        Some(value) => Json(json!({ "name": name, "value": value })).into_response(),
        None => detail(
            StatusCode::NOT_FOUND,
            format!("Parameter '{name}' not found"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::PairingState;
    use crate::ipc::{LogdQueryClient, MavlinkIpcClient, StateIpcClient};
    use crate::state::PairingPaths;
    use serde_json::Value;
    use std::sync::Arc;

    /// Build an `AppState` for a handler test: the parameter cache pointed inside
    /// `dir`, a disconnected state client, and inert paths/clients for the rest.
    /// The read path only touches the cache file, so the state/MAVLink/logd clients
    /// point at absent sockets and are never exercised.
    fn test_state(dir: &std::path::Path) -> AppState {
        let pairing = Arc::new(PairingState::with_path(dir.join("pairing.json")));
        let state = StateIpcClient::disconnected();
        let mavlink = MavlinkIpcClient::new(dir.join("absent-mavlink.sock"));
        let logd = LogdQueryClient::new(dir.join("absent-logd.sock"));
        let pairing_paths = PairingPaths {
            config: dir.join("config.yaml"),
            pairing_json: dir.join("pairing.json"),
            wfb_key_dir: dir.join("wfb"),
            bind_state: dir.join("bind-state.json"),
            profile_conf: dir.join("profile.conf"),
            mesh_role: dir.join("mesh-role"),
        };
        AppState::new(
            pairing,
            state,
            mavlink,
            logd,
            dir.join("board.json"),
            pairing_paths,
            std::sync::Arc::new(crate::dashboard_pin::DashboardPin::with_path(
                dir.join("dashboard-pin.json"),
            )),
            std::sync::Arc::new(crate::mcp::McpTokenStore::with_path(
                dir.join("mcp-token.json"),
            )),
        )
        .with_params_path(dir.join("params.json"))
    }

    /// Write a router-shaped parameter cache at the path `test_state` reads. The
    /// values are `Value` rather than `f64` so a test can pin the integer form.
    fn write_cache(dir: &std::path::Path, entries: &[(&str, Value)]) {
        let doc: serde_json::Map<String, Value> = entries
            .iter()
            .map(|(name, value)| {
                (
                    (*name).to_string(),
                    json!({ "value": value, "param_type": 9, "last_updated": 1.0 }),
                )
            })
            .collect();
        std::fs::write(
            dir.join("params.json"),
            serde_json::to_vec(&Value::Object(doc)).unwrap(),
        )
        .unwrap();
    }

    /// Read a response body as JSON.
    async fn body_json(resp: Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn present_param_is_a_200_with_the_name_and_value() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        write_cache(
            dir.path(),
            &[
                ("WPNAV_SPEED", json!(500.0)),
                ("ATC_RAT_RLL_P", json!(0.135)),
            ],
        );
        let resp = get_param(Path("WPNAV_SPEED".to_string()), State(state)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(
            body,
            json!({ "name": "WPNAV_SPEED", "value": 500.0 }),
            "the 200 body is exactly {{name, value}}"
        );
    }

    #[tokio::test]
    async fn an_integer_valued_param_keeps_its_integer_json_form() {
        // Not coerced to a float, matching the full-list route which clones the
        // cached value verbatim.
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        write_cache(dir.path(), &[("SYSID_THISMAV", json!(1))]);
        let resp = get_param(Path("SYSID_THISMAV".to_string()), State(state)).await;
        let body = body_json(resp).await;
        assert_eq!(body["value"], json!(1));
        assert!(
            body["value"].is_i64() || body["value"].is_u64(),
            "integer form preserved"
        );
    }

    #[tokio::test]
    async fn absent_param_is_a_404_with_the_fastapi_message() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        write_cache(dir.path(), &[("WPNAV_SPEED", json!(500.0))]);
        let resp = get_param(Path("NO_SUCH_PARAM".to_string()), State(state)).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = body_json(resp).await;
        assert_eq!(
            body,
            json!({ "detail": "Parameter 'NO_SUCH_PARAM' not found" }),
            "the 404 body carries the byte-exact FastAPI not-found detail"
        );
    }

    #[tokio::test]
    async fn an_absent_cache_file_is_the_404_not_found_case_never_a_500() {
        // No cache file at all (a fresh boot, or no router running) reads as
        // not-found, the same 404 the FastAPI route returns with its empty
        // in-process cache.
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let resp = get_param(Path("WPNAV_SPEED".to_string()), State(state)).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = body_json(resp).await;
        assert_eq!(
            body,
            json!({ "detail": "Parameter 'WPNAV_SPEED' not found" })
        );
    }

    #[tokio::test]
    async fn a_params_blob_left_on_the_snapshot_is_not_read() {
        // The producer no longer publishes one; serving a stale blob as a fallback
        // would answer from a map nothing writes any more.
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        state.state.set_snapshot_for_test(json!({
            "fc_connected": true,
            "params": { "GHOST": 1.0 },
        }));
        let resp = get_param(Path("GHOST".to_string()), State(state)).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
