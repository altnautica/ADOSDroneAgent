//! Flight-controller parameter routes.
//!
//! `GET /api/params` is the full cached FC parameter list. The parameter MAP comes
//! from the MAVLink router's on-disk cache ([`crate::param_store`]); the
//! sweep-progress envelope around it comes from the vehicle-state IPC snapshot the
//! router publishes on `/run/ados/state.sock`.
//!
//! The map used to ride in that snapshot too, as a `params` blob republished on
//! every 100 ms state publish with no delta and no size cap — ~24 KB against
//! ArduPilot's ~700 parameters, which alone made a relayed `/api/telemetry` need
//! ~21 aux-lane fragments and deliver 85% of the time. The snapshot now carries
//! only a `param_generation` counter; the map is read from the file the router
//! already persists atomically, and a consumer refetches this route once when the
//! counter moves. The native front sits in front of the standalone API process,
//! which holds no in-process parameter cache or vehicle-state object, so that file
//! is the only production-reachable source.
//!
//! The counts and flags stay on the snapshot, because they are live link state
//! rather than cache contents: the router writes the cached/expected counts
//! (`param_cached_count` / `param_expected_count`) and the three param-sweep flags
//! (`param_priming` / `param_sweep_timed_out` / `param_sweep_send_failed`) there,
//! and this route reads them straight back out.
//!
//! The body carries a `priming` flag and a `progress` block so the dashboard can
//! render an in-flight progress bar between the `PARAM_REQUEST_LIST` sweep firing
//! and the cache catching up to the FC's advertised total. `priming_timeout`
//! flips true when the FC stayed silent past the sweep deadline;
//! `priming_send_failed` flips true when the `PARAM_REQUEST_LIST` send itself
//! raised at the link layer. The dashboard reads these to swap the spinner for an
//! actionable empty state instead of looping forever.
//!
//! With no router running (an absent snapshot and no cache file), the route
//! returns the same empty shape the FastAPI route returns when its own source is
//! empty: `params:{}`, the counts zero, the flags false, and a `progress` of
//! `{got:0, expected:0}` — a valid, GCS-parseable body, never a 500.
//!
//! `GET /api/params/{name}` (a single parameter by name) is a path-param route
//! and is NOT served here: the front's native-route matcher is exact-match only,
//! so it falls through to the reverse proxy.

use axum::extract::State;
use axum::Json;
use serde_json::{json, Map, Value};

use crate::state::AppState;

/// `GET /api/params` → the full cached FC parameter list plus the sweep-progress
/// envelope: the map from the router's on-disk cache, the envelope from the
/// vehicle-state IPC snapshot.
///
/// The body is `{"params": {name: value}, "count": …, "cached": …, "priming": …,
/// "priming_timeout": …, "priming_send_failed": …, "progress": {"got": …,
/// "expected": …}}`. `count` is the FC-advertised total when known, else the
/// cached count; `cached` is how many parameters have landed; `progress.got` is
/// the cached count and `progress.expected` the advertised total. An absent
/// snapshot or an absent cache file degrades every field to its empty/zero/false
/// default rather than failing — guaranteed-200, never 500.
pub async fn get_all_params(State(state): State<AppState>) -> Json<Value> {
    let snapshot = state.state.snapshot();
    let params = crate::param_store::read_param_blob(&state.params_path);
    Json(build_params_body(snapshot.as_ref(), params))
}

/// Build the parameter-list body from the cached map plus a state-IPC snapshot,
/// mirroring the FastAPI route's IPC-snapshot fallback (the production path on the
/// multi-process supervisor, where no in-process param cache or vehicle state
/// exists).
///
/// * `params` is the flattened `{name: value}` map [`crate::param_store`] read off
///   disk, already degraded to empty for a missing or unreadable cache.
/// * `cached` / `expected` are the snapshot's `param_cached_count` /
///   `param_expected_count` integers, each defaulting to `0` when absent, null,
///   non-numeric, or already zero (the Python `int(ipc.get(k, 0) or 0)` coercion,
///   truncating toward zero).
/// * `count` is `expected` when it is non-zero, else `cached` (Python
///   `expected or cached`).
/// * the three priming flags come from the snapshot when it carries
///   `param_priming`, else all three are `false` (the Python `_resolve_priming_flags`
///   fallback when the snapshot has not yet reported the sweep state).
fn build_params_body(snapshot: Option<&Value>, params: Map<String, Value>) -> Value {
    let obj = snapshot.and_then(Value::as_object);

    let cached = int_or_zero(obj, "param_cached_count");
    let expected = int_or_zero(obj, "param_expected_count");
    let count = if expected != 0 { expected } else { cached };

    let (priming, priming_timeout, priming_send_failed) = resolve_priming_flags(obj);

    json!({
        "params": Value::Object(params),
        "count": count,
        "cached": cached,
        "priming": priming,
        "priming_timeout": priming_timeout,
        "priming_send_failed": priming_send_failed,
        "progress": { "got": cached, "expected": expected },
    })
}

/// Resolve the three param-sweep flags, preferring the snapshot. When the snapshot
/// carries `param_priming`, the flags are read from it (each coerced to a bool,
/// mirroring the Python `bool(ipc.get(...))`); otherwise all three are `false`
/// (the Python fallback that returns `getattr(fc, ..., False)` against a `None` FC
/// handle on the API process). Returns `(priming, priming_timeout,
/// priming_send_failed)`.
fn resolve_priming_flags(obj: Option<&Map<String, Value>>) -> (bool, bool, bool) {
    let Some(map) = obj else {
        return (false, false, false);
    };
    if !map.contains_key("param_priming") {
        return (false, false, false);
    }
    (
        truthy(map.get("param_priming")),
        truthy(map.get("param_sweep_timed_out")),
        truthy(map.get("param_sweep_send_failed")),
    )
}

/// Read an integer count out of the snapshot, mirroring `int(ipc.get(key, 0) or
/// 0)`: a numeric value truncates toward zero; an absent, null, non-numeric, or
/// already-zero value yields `0`. (The `or 0` in Python only substitutes when the
/// fetched value is falsy, which for these numeric counts is exactly the
/// zero/null/absent case — and the router only ever writes integers here, so the
/// truncation is a no-op in production, kept only for faithful coercion.)
fn int_or_zero(obj: Option<&Map<String, Value>>, key: &str) -> i64 {
    let Some(value) = obj.and_then(|m| m.get(key)) else {
        return 0;
    };
    match value {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i
            } else if let Some(f) = n.as_f64() {
                f.trunc() as i64
            } else {
                0
            }
        }
        _ => 0,
    }
}

/// Python truthiness for the boolean snapshot flags: a JSON `true` is true; a
/// `false`, null, `0`/`0.0`, or absent value is false. (The router writes plain
/// booleans here, so the bool branch is the only one that fires in production;
/// the rest keep the coercion faithful to `bool(...)`.)
fn truthy(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::PairingState;
    use crate::ipc::{LogdQueryClient, MavlinkIpcClient, StateIpcClient};
    use crate::state::PairingPaths;
    use std::path::Path;
    use std::sync::Arc;

    /// The two-parameter cache the golden body is built from.
    fn two_params() -> Map<String, Value> {
        json!({ "WPNAV_SPEED": 500.0, "ATC_RAT_RLL_P": 0.135 })
            .as_object()
            .cloned()
            .unwrap()
    }

    /// Build an `AppState` for a handler test: a disconnected state client (the
    /// test primes its snapshot directly), the parameter cache pointed at `dir`,
    /// and inert paths/clients for the rest.
    fn test_state(dir: &Path) -> AppState {
        let pairing = Arc::new(PairingState::with_path(dir.join("pairing.json")));
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
            StateIpcClient::disconnected(),
            MavlinkIpcClient::new(dir.join("absent-mavlink.sock")),
            LogdQueryClient::new(dir.join("absent-logd.sock")),
            dir.join("board.json"),
            pairing_paths,
            Arc::new(crate::dashboard_pin::DashboardPin::with_path(
                dir.join("dashboard-pin.json"),
            )),
            Arc::new(crate::mcp::McpTokenStore::with_path(
                dir.join("mcp-token.json"),
            )),
        )
        .with_params_path(dir.join("params.json"))
    }

    /// Write a router-shaped parameter cache at the path `test_state` reads.
    fn write_cache(dir: &Path, entries: &[(&str, f64)]) {
        let doc: Map<String, Value> = entries
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

    /// The golden production body for a representative snapshot: a two-parameter
    /// cache mid-sweep, with the FC having advertised 700 total. This is the exact
    /// JSON the FastAPI `get_all_params` route returns on the multi-process
    /// supervisor (no in-process cache, the IPC-snapshot fallback). `assert_eq!`
    /// on `serde_json::Value` compares the maps semantically (same keys, values,
    /// types, nesting), which is the parity contract the conformance harness and
    /// the GCS both rely on.
    #[test]
    fn golden_parity_for_a_mid_sweep_snapshot() {
        let snapshot = json!({
            "fc_connected": true,
            "fc_port": "/dev/ttyACM0",
            "fc_baud": 115200,
            "service_uptime": 42.0,
            "param_generation": 2,
            "param_cached_count": 2,
            "param_expected_count": 700,
            "param_priming": true,
            "param_sweep_timed_out": false,
            "param_sweep_send_failed": false,
        });
        let body = build_params_body(Some(&snapshot), two_params());
        let expected = json!({
            "params": {
                "WPNAV_SPEED": 500.0,
                "ATC_RAT_RLL_P": 0.135,
            },
            "count": 700,
            "cached": 2,
            "priming": true,
            "priming_timeout": false,
            "priming_send_failed": false,
            "progress": { "got": 2, "expected": 700 },
        });
        assert_eq!(body, expected);
    }

    #[test]
    fn absent_snapshot_is_the_empty_default_shape() {
        let body = build_params_body(None, Map::new());
        let expected = json!({
            "params": {},
            "count": 0,
            "cached": 0,
            "priming": false,
            "priming_timeout": false,
            "priming_send_failed": false,
            "progress": { "got": 0, "expected": 0 },
        });
        assert_eq!(body, expected);
    }

    #[test]
    fn count_falls_back_to_cached_when_expected_is_zero() {
        // The FC has not yet advertised a total (expected 0) but parameters have
        // already landed: count must report the cached number, not zero.
        let snapshot = json!({
            "param_cached_count": 5,
            "param_expected_count": 0,
        });
        let body = build_params_body(Some(&snapshot), two_params());
        assert_eq!(body["count"], json!(5));
        assert_eq!(body["cached"], json!(5));
        assert_eq!(body["progress"], json!({ "got": 5, "expected": 0 }));
    }

    #[test]
    fn count_prefers_expected_when_non_zero() {
        let snapshot = json!({
            "param_cached_count": 3,
            "param_expected_count": 700,
        });
        let body = build_params_body(Some(&snapshot), Map::new());
        assert_eq!(body["count"], json!(700));
    }

    #[test]
    fn priming_flags_default_false_when_the_snapshot_omits_them() {
        // No param_priming key in the snapshot → all three flags read false,
        // even when the *_timed_out / *_send_failed keys happen to be present.
        let snapshot = json!({
            "param_sweep_timed_out": true,
            "param_sweep_send_failed": true,
        });
        let body = build_params_body(Some(&snapshot), Map::new());
        assert_eq!(body["priming"], json!(false));
        assert_eq!(body["priming_timeout"], json!(false));
        assert_eq!(body["priming_send_failed"], json!(false));
    }

    #[test]
    fn priming_flags_read_from_the_snapshot_when_present() {
        let snapshot = json!({
            "param_priming": false,
            "param_sweep_timed_out": true,
            "param_sweep_send_failed": false,
        });
        let body = build_params_body(Some(&snapshot), Map::new());
        assert_eq!(body["priming"], json!(false));
        assert_eq!(body["priming_timeout"], json!(true));
        assert_eq!(body["priming_send_failed"], json!(false));
    }

    #[test]
    fn counts_coerce_null_and_missing_to_zero() {
        // A null count, or a missing count, both read as 0 (the Python `or 0`).
        let snapshot = json!({ "param_cached_count": Value::Null });
        let body = build_params_body(Some(&snapshot), Map::new());
        assert_eq!(body["cached"], json!(0));
        assert_eq!(body["count"], json!(0));
        assert_eq!(body["progress"], json!({ "got": 0, "expected": 0 }));
    }

    #[test]
    fn float_counts_truncate_toward_zero() {
        // The router writes integers, but a float count truncates like Python int().
        let obj = json!({ "k": 2.9 });
        assert_eq!(int_or_zero(obj.as_object(), "k"), 2);
        // An absent / non-numeric key reads as zero.
        assert_eq!(int_or_zero(obj.as_object(), "missing"), 0);
        let strobj = json!({ "k": "nope" });
        assert_eq!(int_or_zero(strobj.as_object(), "k"), 0);
    }

    #[test]
    fn the_body_carries_exactly_the_seven_expected_keys() {
        // serde_json::Map is a BTreeMap on this build (no preserve_order feature),
        // so it sorts keys — the body is compared semantically, not by literal key
        // order. What the contract fixes is the *set* of keys present.
        let body = build_params_body(
            Some(&json!({
                "param_cached_count": 1,
                "param_expected_count": 2,
                "param_priming": true,
            })),
            Map::new(),
        );
        let obj = body.as_object().unwrap();
        for key in [
            "params",
            "count",
            "cached",
            "priming",
            "priming_timeout",
            "priming_send_failed",
            "progress",
        ] {
            assert!(obj.contains_key(key), "body must carry {key}");
        }
        assert_eq!(obj.len(), 7, "body carries exactly the seven envelope keys");
    }

    // ── the handler, against a real cache file ─────────────────────────────────

    #[tokio::test]
    async fn the_route_serves_the_on_disk_cache() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        write_cache(
            dir.path(),
            &[("WPNAV_SPEED", 500.0), ("ATC_RAT_RLL_P", 0.135)],
        );
        // The snapshot carries the counter and the counts, NOT the map.
        state.state.set_snapshot_for_test(json!({
            "param_generation": 7,
            "param_cached_count": 2,
            "param_expected_count": 700,
        }));

        let Json(body) = get_all_params(State(state)).await;
        assert_eq!(body["params"]["WPNAV_SPEED"], json!(500.0));
        assert_eq!(body["params"]["ATC_RAT_RLL_P"], json!(0.135));
        assert_eq!(body["cached"], json!(2));
        assert_eq!(body["count"], json!(700));
    }

    #[tokio::test]
    async fn a_missing_cache_file_is_an_empty_map_not_a_failure() {
        // No file at all: a fresh boot before the router has persisted anything.
        // The envelope still reports whatever the snapshot knows, and the body is
        // the same 200 the route always returned for an empty source.
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        state
            .state
            .set_snapshot_for_test(json!({ "param_cached_count": 0 }));

        let Json(body) = get_all_params(State(state)).await;
        assert_eq!(body["params"], json!({}));
        assert_eq!(body["count"], json!(0));
        assert_eq!(body["priming"], json!(false));
    }

    #[tokio::test]
    async fn the_route_does_not_read_a_params_blob_off_the_snapshot() {
        // The blob is gone from the producer, so a stale one appearing on the
        // snapshot must NOT be served: the file is the single source. This fails
        // on the plausible bug of leaving the old snapshot read in place as a
        // fallback, which would serve a map nothing writes any more.
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        state.state.set_snapshot_for_test(json!({
            "params": { "GHOST": 1.0 },
            "param_cached_count": 1,
        }));

        let Json(body) = get_all_params(State(state)).await;
        assert_eq!(body["params"], json!({}));
    }
}
