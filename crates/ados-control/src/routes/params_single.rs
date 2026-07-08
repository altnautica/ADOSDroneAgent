//! Single flight-controller parameter read route.
//!
//! `GET /api/params/{name}` returns one cached FC parameter by name. It reads the
//! same source the full parameter list ([`crate::routes::params`]) reads — the
//! vehicle-state IPC snapshot's `params` blob the MAVLink router publishes on
//! `/run/ados/state.sock` — and filters it to the one requested name. The native
//! front sits in front of the standalone API process, which holds no in-process
//! parameter cache or vehicle-state object, so the IPC snapshot's `params` blob (a
//! `{name: value}` object) is the only production-reachable source; this route
//! reads `params[name]` straight back out.
//!
//! When the name is present the body is `{"name": <name>, "value": <value>}` with
//! the value passed through verbatim from the snapshot (preserving its exact JSON
//! number form, the same way the full-list route clones the blob). When the name
//! is absent — an empty / non-object / absent blob, or a name not in it — the route
//! returns the FastAPI 404 `{"detail": "Parameter '<name>' not found"}`, the exact
//! status and message the proxied FastAPI route raised.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::routes::detail;
use crate::state::AppState;

/// `GET /api/params/{name}` → `{"name", "value"}` for a cached FC parameter, or a
/// 404 `{"detail"}` when the name is not in the cache.
///
/// Reads the value from the state-IPC snapshot's `params` blob (the same source
/// the full-list read uses). A name present in the blob returns `200` with the
/// value verbatim; any absent / non-object / missing-name case returns the FastAPI
/// `404` with the byte-identical not-found message. Never panics on a seam error:
/// an absent snapshot is the not-found case, never a 500.
pub async fn get_param(Path(name): Path<String>, State(state): State<AppState>) -> Response {
    let snapshot = state.state.snapshot();
    match lookup_param(snapshot.as_ref(), &name) {
        Some(value) => Json(json!({ "name": name, "value": value })).into_response(),
        None => detail(
            StatusCode::NOT_FOUND,
            format!("Parameter '{name}' not found"),
        ),
    }
}

/// Read `params[name]` out of the state-IPC snapshot, returning the value verbatim
/// (its exact JSON number form preserved) or `None` when it cannot be resolved.
///
/// `None` is returned when the snapshot is absent, is not a JSON object, carries no
/// `params` blob, the `params` blob is not a JSON object, or the name is absent
/// from it — every one of which the FastAPI route answers as the same 404 (its
/// `param_cache`/`vehicle_state` both empty on the standalone API process). The
/// value is cloned through unchanged so an integer stays an integer and a float
/// stays a float, matching the full-list route which clones the whole blob.
fn lookup_param(snapshot: Option<&Value>, name: &str) -> Option<Value> {
    snapshot
        .and_then(Value::as_object)
        .and_then(|m| m.get("params"))
        .and_then(Value::as_object)
        .and_then(|params| params.get(name))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::PairingState;
    use crate::ipc::{LogdQueryClient, MavlinkIpcClient, StateIpcClient};
    use crate::state::PairingPaths;
    use std::sync::Arc;

    /// Build an `AppState` for a handler test: a disconnected state client (the
    /// test primes its snapshot directly) and inert paths/clients for the rest. The
    /// read path only touches the state client, so the MAVLink/logd clients point at
    /// absent sockets and are never exercised.
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
        )
    }

    /// Read a response body as JSON.
    async fn body_json(resp: Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    // ── lookup_param ───────────────────────────────────────────────────────────

    #[test]
    fn lookup_reads_a_present_param_verbatim() {
        let snap = json!({
            "params": {
                "WPNAV_SPEED": 500.0,
                "ATC_RAT_RLL_P": 0.135,
            }
        });
        // A float param comes back as the same float value.
        assert_eq!(lookup_param(Some(&snap), "WPNAV_SPEED"), Some(json!(500.0)));
        assert_eq!(
            lookup_param(Some(&snap), "ATC_RAT_RLL_P"),
            Some(json!(0.135))
        );
    }

    #[test]
    fn lookup_preserves_an_integer_value_form() {
        // An integer-valued param keeps its integer JSON form (not coerced to a
        // float), matching the full-list route which clones the blob verbatim.
        let snap = json!({ "params": { "SYSID_THISMAV": 1 } });
        let value = lookup_param(Some(&snap), "SYSID_THISMAV").unwrap();
        assert_eq!(value, json!(1));
        assert!(value.is_i64() || value.is_u64(), "integer form preserved");
    }

    #[test]
    fn lookup_is_none_for_an_absent_name() {
        let snap = json!({ "params": { "WPNAV_SPEED": 500.0 } });
        assert_eq!(lookup_param(Some(&snap), "DOES_NOT_EXIST"), None);
    }

    #[test]
    fn lookup_is_none_for_an_absent_snapshot_or_blob() {
        // Absent snapshot, no params key, and a non-object params blob all read None
        // — every one the FastAPI route answers as the same 404.
        assert_eq!(lookup_param(None, "WPNAV_SPEED"), None);
        assert_eq!(
            lookup_param(Some(&json!({ "fc_connected": true })), "WPNAV_SPEED"),
            None
        );
        for blob in [json!(null), json!([1, 2, 3]), json!("nope")] {
            let snap = json!({ "params": blob });
            assert_eq!(
                lookup_param(Some(&snap), "WPNAV_SPEED"),
                None,
                "blob {blob} should read as not-found"
            );
        }
        // An empty params blob → the name is absent.
        assert_eq!(
            lookup_param(Some(&json!({ "params": {} })), "WPNAV_SPEED"),
            None
        );
    }

    // ── the handler ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn present_param_is_a_200_with_the_name_and_value() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        state.state.set_snapshot_for_test(json!({
            "fc_connected": true,
            "params": { "WPNAV_SPEED": 500.0 },
        }));
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
    async fn absent_param_is_a_404_with_the_fastapi_message() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        state
            .state
            .set_snapshot_for_test(json!({ "fc_connected": true, "params": {} }));
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
    async fn an_absent_snapshot_is_the_404_not_found_case_never_a_500() {
        // A disconnected state client (no snapshot primed) reads as not-found, the
        // same 404 the FastAPI route returns with its empty in-process cache.
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
}
