//! `GET /api/plugins/{plugin_id}/state` — a plugin's latest published state.
//!
//! A plugin publishes its own state on its own namespaced topics (a
//! `follow.state`-style read-back). The plugin host writes the latest event per
//! topic into a small JSON sidecar at `<plugin_socket_dir>/<plugin_id>-state.json`
//! on each authorized publish; this route reads that sidecar back and serves it
//! to the LAN-paired GCS, which polls it to light up the Skill Bar state ring and
//! the plugin's detail tab live.
//!
//! The body is the sidecar JSON unchanged: a top-level object keyed by topic,
//! each value `{ "payload": <json>, "ts_ms": <int> }`. An absent sidecar (the
//! plugin has not published, or is not running) is a `404`, and a sidecar that has
//! not been touched in a while (its file mtime is older than the staleness window)
//! is also a `404` so the GCS does not act on a state the plugin is no longer
//! reporting. Any read fault degrades to `404` as well — never a `500`.
//!
//! This is a `read`, served with the front's native auth posture (key-gated when
//! the agent is paired). The plugin socket dir defaults to `/run/ados/plugins`,
//! overridable via `ADOS_PLUGIN_SOCKET_DIR` (the same env the plugin-host daemon
//! reads), so both daemons resolve the identical path.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use axum::extract::Path as AxumPath;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::Value;

use crate::routes::detail;

/// How stale a sidecar may be before the route treats it as absent. A plugin
/// that stops publishing (disabled / crashed / no longer reporting) leaves its
/// last sidecar on disk; beyond this window the route 404s rather than serving a
/// frozen state the GCS would act on.
const STALE_AFTER: Duration = Duration::from_secs(10);

/// The per-plugin socket directory the plugin-host daemon binds its sockets and
/// writes its state sidecars in. Defaults to `/run/ados/plugins`, overridable via
/// `ADOS_PLUGIN_SOCKET_DIR` (the same env the daemon reads) so both agree.
fn plugin_socket_dir() -> PathBuf {
    PathBuf::from(
        std::env::var("ADOS_PLUGIN_SOCKET_DIR").unwrap_or_else(|_| "/run/ados/plugins".to_string()),
    )
}

/// `GET /api/plugins/{plugin_id}/state` → the plugin's latest published state.
///
/// Reads `<plugin_socket_dir>/<plugin_id>-state.json` and returns it as JSON.
/// `404` when the sidecar is absent, stale (mtime older than [`STALE_AFTER`]),
/// unreadable, or malformed; never a `500`.
pub async fn get_plugin_state(AxumPath(plugin_id): AxumPath<String>) -> Response {
    read_plugin_state(&plugin_socket_dir(), &plugin_id, SystemTime::now())
}

/// The read logic against an explicit socket dir + a reference "now", so a test
/// can point it at a temp dir and drive the staleness check deterministically.
fn read_plugin_state(socket_dir: &std::path::Path, plugin_id: &str, now: SystemTime) -> Response {
    let path = socket_dir.join(format!("{plugin_id}-state.json"));

    // Absent / unreadable metadata → 404 (the plugin has published nothing, or
    // is not running). A metadata error is the absent case, not a server fault.
    let Ok(meta) = std::fs::metadata(&path) else {
        return not_found(plugin_id);
    };

    // Staleness gate: if the file has not been written within the window, the
    // plugin is no longer reporting; treat the state as absent. A clock that
    // cannot resolve the mtime (or an mtime in the future) is treated as fresh
    // rather than spuriously stale.
    if let Ok(modified) = meta.modified() {
        if let Ok(age) = now.duration_since(modified) {
            if age > STALE_AFTER {
                return not_found(plugin_id);
            }
        }
    }

    let Ok(text) = std::fs::read_to_string(&path) else {
        return not_found(plugin_id);
    };
    let Ok(doc) = serde_json::from_str::<Value>(&text) else {
        // A torn / malformed sidecar is absent-equivalent, never a 500.
        return not_found(plugin_id);
    };
    // Only an object is a valid sidecar; anything else degrades to absent.
    if !doc.is_object() {
        return not_found(plugin_id);
    }

    (StatusCode::OK, Json(doc)).into_response()
}

/// The `404` body, FastAPI-shaped (`{"detail": "..."}`), matching the rest of the
/// native surface.
fn not_found(plugin_id: &str) -> Response {
    detail(
        StatusCode::NOT_FOUND,
        format!("no published state for plugin {plugin_id}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    async fn body_json(resp: Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn write_sidecar(dir: &std::path::Path, plugin_id: &str, body: &Value) {
        std::fs::write(
            dir.join(format!("{plugin_id}-state.json")),
            serde_json::to_string(body).unwrap(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn serves_a_fresh_sidecar_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let body = json!({
            "follow.state": {
                "payload": { "active": true, "lock_state": "locked", "commanding": true },
                "ts_ms": 1234,
            }
        });
        write_sidecar(dir.path(), "com.example.follow", &body);

        let resp = read_plugin_state(dir.path(), "com.example.follow", SystemTime::now());
        assert_eq!(resp.status(), StatusCode::OK);
        let got = body_json(resp).await;
        assert_eq!(got, body);
    }

    #[tokio::test]
    async fn an_absent_sidecar_is_a_404() {
        let dir = tempfile::tempdir().unwrap();
        let resp = read_plugin_state(dir.path(), "com.example.absent", SystemTime::now());
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let got = body_json(resp).await;
        assert_eq!(
            got,
            json!({ "detail": "no published state for plugin com.example.absent" })
        );
    }

    #[tokio::test]
    async fn a_stale_sidecar_is_a_404() {
        let dir = tempfile::tempdir().unwrap();
        write_sidecar(
            dir.path(),
            "com.example.stale",
            &json!({ "follow.state": { "payload": {}, "ts_ms": 1 } }),
        );
        // Drive "now" far past the file's just-written mtime so the staleness
        // gate fires deterministically without sleeping.
        let future = SystemTime::now() + STALE_AFTER + Duration::from_secs(5);
        let resp = read_plugin_state(dir.path(), "com.example.stale", future);
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_fresh_sidecar_just_under_the_window_is_served() {
        let dir = tempfile::tempdir().unwrap();
        write_sidecar(
            dir.path(),
            "com.example.fresh",
            &json!({ "follow.state": { "payload": { "active": false }, "ts_ms": 9 } }),
        );
        // Now is only a moment after the write — comfortably inside the window.
        let resp = read_plugin_state(dir.path(), "com.example.fresh", SystemTime::now());
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_malformed_sidecar_is_a_404_not_a_500() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("com.example.bad-state.json"),
            b"not json {{{",
        )
        .unwrap();
        let resp = read_plugin_state(dir.path(), "com.example.bad", SystemTime::now());
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_non_object_sidecar_is_a_404() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("com.example.arr-state.json"), b"[1, 2, 3]").unwrap();
        let resp = read_plugin_state(dir.path(), "com.example.arr", SystemTime::now());
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
