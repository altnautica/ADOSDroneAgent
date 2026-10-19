//! `GET /api/v1/clock`: the host timebase and its disciplining sources, from
//! the `ados-clock` sidecar.

use std::time::SystemTime;

use axum::extract::State;
use axum::Json;
use serde_json::{json, Map, Value};

use super::{fresh_map, with_present, SensingPaths, FRESH};
use crate::state::AppState;

pub async fn get_clock(State(state): State<AppState>) -> Json<Value> {
    let paths = SensingPaths::from_state(&state);
    let body = tokio::task::spawn_blocking(move || clock_body(&paths, SystemTime::now()))
        .await
        .unwrap_or_else(|_| with_present(false, empty()));
    Json(body)
}

pub(crate) fn clock_body(paths: &SensingPaths, now: SystemTime) -> Value {
    match fresh_map(&paths.registered("clock"), Some(FRESH), now) {
        Some(m) => with_present(true, m),
        None => with_present(false, empty()),
    }
}

/// The clock body with the service absent: nothing measured.
fn empty() -> Map<String, Value> {
    let Value::Object(m) = json!({
        "timebase": "host_monotonic",
        "locked": false,
        "sigma_ns": -1,
        "t_ns": 0,
        "utc_ns": null,
        "sources": [],
    }) else {
        unreachable!()
    };
    m
}

/// The clock source sigma for a sensor origin (`hub`, `fc`, `sim`, `gnss`),
/// `-1` when the clock is absent or the source unmeasured.
pub(crate) fn source_sigma(clock: &Value, origin: &str) -> i64 {
    clock["sources"]
        .as_array()
        .and_then(|s| s.iter().find(|x| x["id"] == origin))
        .and_then(|x| x["sigma_ns"].as_i64())
        .unwrap_or(-1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn paths(dir: &std::path::Path) -> SensingPaths {
        SensingPaths {
            run_dir: dir.to_path_buf(),
            etc_dir: dir.to_path_buf(),
        }
    }

    const SIDECAR: &str = r#"{"version":1,"timebase":"hub_freerun","locked":true,"sigma_ns":420000,
        "t_ns":123456789000,"utc_ns":null,
        "sources":[{"id":"fc","kind":"fc","locked":false,"offset_ns":0,"skew_ppm":0.0,"sigma_ns":-1,"last_update_ns":0},
                   {"id":"hub","kind":"hub","locked":true,"offset_ns":12000,"skew_ppm":-3.2,"sigma_ns":420000,"last_update_ns":123456700000},
                   {"id":"gnss","kind":"gnss_pps","locked":false,"offset_ns":0,"skew_ppm":0.0,"sigma_ns":-1,"last_update_ns":0},
                   {"id":"sim","kind":"sim","locked":false,"offset_ns":0,"skew_ppm":0.0,"sigma_ns":-1,"last_update_ns":0}]}"#;

    /// Write [`SIDECAR`] where ados-clock writes it under `dir`.
    fn write_clock(dir: &std::path::Path) {
        let path = paths(dir).registered("clock");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, SIDECAR).unwrap();
    }

    #[test]
    fn a_fresh_sidecar_is_served_with_present_and_without_version() {
        let dir = tempfile::tempdir().unwrap();
        write_clock(dir.path());
        let body = clock_body(&paths(dir.path()), SystemTime::now());
        let mut keys: Vec<&str> = body
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["locked", "present", "sigma_ns", "sources", "t_ns", "timebase", "utc_ns"]
        );
        assert_eq!(body["present"], true);
        assert_eq!(body["sources"][1]["kind"], "hub");
        for k in [
            "id",
            "kind",
            "locked",
            "offset_ns",
            "skew_ppm",
            "sigma_ns",
            "last_update_ns",
        ] {
            assert!(body["sources"][0].get(k).is_some(), "source lacks {k}");
        }
        assert_eq!(source_sigma(&body, "hub"), 420000);
        assert_eq!(source_sigma(&body, "vision"), -1);
    }

    #[test]
    fn an_absent_or_stale_clock_is_not_present() {
        let dir = tempfile::tempdir().unwrap();
        let absent = clock_body(&paths(dir.path()), SystemTime::now());
        assert_eq!(absent["present"], false);
        assert_eq!(absent["sigma_ns"], -1);
        assert_eq!(absent["utc_ns"], Value::Null);
        assert_eq!(absent["sources"], json!([]));
        write_clock(dir.path());
        let later = SystemTime::now() + Duration::from_secs(30);
        assert_eq!(clock_body(&paths(dir.path()), later)["present"], false);
    }
}
