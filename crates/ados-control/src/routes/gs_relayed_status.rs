//! Ground-station relayed-node status route.
//!
//! **`GET /api/v1/ground-station/relayed/status`** — what this ground station
//! knows about the nodes it relays over the radio.
//!
//! A node reached only through this ground station has no address of its own, so
//! it pushes a compact status snapshot and an identity frame over the radio's
//! auxiliary lane. The receive process caches those and publishes them to a
//! sidecar; this route serves that sidecar. It is the read seam beside the
//! sibling relayed-config route, which forwards a request the other way.
//!
//! ## Freshness is recomputed here, not trusted
//!
//! The sidecar carries the wall-clock time each frame arrived. This route
//! computes every age against its OWN clock at request time rather than serving
//! the ages the writer precomputed, because the writer's numbers were true when
//! it wrote them and the file can be seconds old by the time it is read. A
//! status that has aged past the window since the write is dropped here even
//! though the writer published it as fresh.
//!
//! Two gates, for two different failures. A sidecar that has not been rewritten
//! recently means the receive process is not running, and the whole surface
//! reads `404` rather than serving its last contents. A peer whose own status
//! has aged out keeps its identity but loses its status block: the node is still
//! known, its readings are not current. Neither case ever serves a stale reading
//! as a live one (operating rule 44).

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};

use crate::routes::detail;
use crate::state::AppState;

/// The relayed-status sidecar filename under the run dir.
const SIDECAR_FILE: &str = "relayed-status.json";

/// How stale the sidecar itself may be before the surface reads as absent.
///
/// The writer rewrites it every couple of seconds, so this is generous enough to
/// ride out a slow tick and tight enough that a stopped receive process is
/// reported as gone rather than as a node that stopped moving.
const SIDECAR_STALE_AFTER_S: f64 = 20.0;

/// Fallback freshness window for a peer's status, used only when the sidecar
/// does not publish its own. The writer does publish it, so this is the
/// compatibility path for a sidecar written by an older receive process.
const DEFAULT_STATUS_STALE_AFTER_S: f64 = 15.0;

fn run_dir() -> PathBuf {
    PathBuf::from(std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string()))
}

fn now_unix() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn is_ground_station(state: &AppState) -> bool {
    let cfg = crate::config::PairingConfig::load_from(&state.pairing_paths.config);
    let (profile, _role) = crate::profile::current_profile_and_role(&cfg.agent.profile);
    profile == "ground-station"
}

fn profile_mismatch() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})),
    )
        .into_response()
}

fn not_running() -> Response {
    detail(
        StatusCode::NOT_FOUND,
        "no relayed-node status available on this node",
    )
}

pub async fn get_relayed_status(State(state): State<AppState>) -> Response {
    if !is_ground_station(&state) {
        return profile_mismatch();
    }
    read_status(&run_dir().join(SIDECAR_FILE), now_unix())
}

/// The path + clock injectable core, so a test drives the freshness gates
/// deterministically against a tempdir.
fn read_status(path: &Path, now: f64) -> Response {
    let Ok(text) = std::fs::read_to_string(path) else {
        return not_running();
    };
    let Ok(doc) = serde_json::from_str::<Value>(&text) else {
        return not_running();
    };
    let Some(obj) = doc.as_object() else {
        return not_running();
    };

    // Best-effort drift signal, then read anyway: the same posture the sibling
    // sidecar readers take.
    let version = obj.get("version").and_then(Value::as_u64).unwrap_or(0) as u16;
    if let Some(expected) = ados_protocol::contracts::sidecar_version("relayed-status") {
        ados_protocol::sidecar::check_sidecar_version("relayed-status", version, expected);
    }

    // Gate one: is the writer alive? A sidecar left behind by a stopped receive
    // process must not keep answering for the nodes it used to hear.
    let written_at = obj
        .get("wall_time_unix")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    if written_at <= 0.0 || now - written_at > SIDECAR_STALE_AFTER_S {
        return not_running();
    }

    let stale_after = obj
        .get("status_stale_after_s")
        .and_then(Value::as_f64)
        .filter(|v| *v > 0.0)
        .unwrap_or(DEFAULT_STATUS_STALE_AFTER_S);

    let peers: Vec<Value> = obj
        .get("peers")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|p| refresh_peer(p, now, stale_after))
                .collect()
        })
        .unwrap_or_default();

    (
        StatusCode::OK,
        Json(json!({
            "peers": peers,
            "peer_count": peers.len(),
            "status_stale_after_s": stale_after,
            "generated_at_unix": now,
            "source_written_at_unix": written_at,
        })),
    )
        .into_response()
}

/// Re-age one peer against the request-time clock.
///
/// The writer's `status_age_s` was true when it wrote; by read time the file may
/// be seconds old. So the age is recomputed from the arrival timestamp and the
/// status block is dropped if it has gone stale since, even though the writer
/// published it. A peer with no timestamps at all is dropped: an entry that
/// cannot be aged cannot be shown to be current.
fn refresh_peer(peer: &Value, now: f64, stale_after: f64) -> Option<Value> {
    let src = peer.as_object()?;
    let mut out = Map::new();

    // Identity and provenance carry over unchanged.
    for key in [
        "device_id",
        "name",
        "profile",
        "agent_version",
        "seq",
        "status_frames",
        "out_of_order",
    ] {
        if let Some(v) = src.get(key) {
            out.insert(key.to_string(), v.clone());
        }
    }

    let identity_at = src.get("identity_at_unix").and_then(Value::as_f64);
    let status_at = src.get("status_at_unix").and_then(Value::as_f64);
    if identity_at.is_none() && status_at.is_none() {
        return None;
    }

    if let Some(at) = identity_at {
        out.insert("identity_at_unix".into(), json!(at));
        out.insert("identity_age_s".into(), json!(round2(now - at)));
    }

    let status_fresh = match status_at {
        Some(at) => {
            out.insert("status_at_unix".into(), json!(at));
            out.insert("status_age_s".into(), json!(round2(now - at)));
            now - at <= stale_after
        }
        None => false,
    };
    out.insert("status_fresh".into(), json!(status_fresh));
    if status_fresh {
        if let Some(status) = src.get("status") {
            out.insert("status".into(), status.clone());
        }
    }

    Some(Value::Object(out))
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    async fn body_of(resp: Response) -> (StatusCode, Value) {
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// A sidecar written `age` seconds before `now`, carrying one peer whose
    /// status arrived `status_age` seconds before `now`.
    fn sidecar(now: f64, age: f64, status_age: f64) -> Value {
        json!({
            "version": 1,
            "wall_time_unix": now - age,
            "status_stale_after_s": 15.0,
            "peers": [{
                "device_id": "drone-a",
                "name": "Alpha",
                "profile": "drone",
                "agent_version": "1.2.3",
                "seq": 42,
                "status_frames": 40,
                "out_of_order": 1,
                "identity_at_unix": now - status_age,
                "status_at_unix": now - status_age,
                // The writer's own verdict, deliberately optimistic here so the
                // test proves the route does not simply trust it.
                "status_fresh": true,
                "status": {"cpu_percent": 12.5, "fc_connected": true},
            }],
        })
    }

    #[tokio::test]
    async fn a_fresh_sidecar_serves_the_peer_with_its_status() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SIDECAR_FILE);
        let now = 1_700_000_000.0;
        std::fs::write(&path, sidecar(now, 1.0, 2.0).to_string()).unwrap();

        let (status, body) = body_of(read_status(&path, now)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["peer_count"], 1);
        let p = &body["peers"][0];
        assert_eq!(p["device_id"], "drone-a");
        assert_eq!(p["name"], "Alpha");
        assert_eq!(p["status_fresh"], true);
        assert_eq!(p["status"]["cpu_percent"], 12.5);
        assert_eq!(p["status_age_s"], 2.0);
    }

    #[tokio::test]
    async fn a_status_that_aged_out_since_the_write_is_not_served_as_current() {
        // The point of recomputing: the writer marked this peer fresh, and by
        // read time it is not. Trusting the file would serve a dead reading.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SIDECAR_FILE);
        let now = 1_700_000_000.0;
        std::fs::write(&path, sidecar(now, 1.0, 60.0).to_string()).unwrap();

        let (status, body) = body_of(read_status(&path, now)).await;
        assert_eq!(status, StatusCode::OK);
        let p = &body["peers"][0];
        assert_eq!(p["status_fresh"], false);
        assert!(
            p.get("status").is_none(),
            "a stale reading must not be served"
        );
        // The node is still known, and its age is reported honestly.
        assert_eq!(p["device_id"], "drone-a");
        assert_eq!(p["status_age_s"], 60.0);
    }

    #[tokio::test]
    async fn a_dead_writer_reads_absent_rather_than_serving_its_last_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SIDECAR_FILE);
        let now = 1_700_000_000.0;
        std::fs::write(
            &path,
            sidecar(now, SIDECAR_STALE_AFTER_S + 5.0, 1.0).to_string(),
        )
        .unwrap();
        assert_eq!(read_status(&path, now).status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn absent_malformed_and_untimestamped_sidecars_read_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SIDECAR_FILE);
        assert_eq!(read_status(&path, 1.0).status(), StatusCode::NOT_FOUND);

        std::fs::write(&path, b"{not json").unwrap();
        assert_eq!(read_status(&path, 1.0).status(), StatusCode::NOT_FOUND);

        std::fs::write(&path, b"[]").unwrap();
        assert_eq!(read_status(&path, 1.0).status(), StatusCode::NOT_FOUND);

        // A document with no write timestamp cannot be aged, so it cannot be
        // shown to be current.
        std::fs::write(&path, br#"{"peers":[]}"#).unwrap();
        assert_eq!(read_status(&path, 1.0).status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_fresh_sidecar_with_no_peers_is_an_honest_empty_list() {
        // Distinct from a dead writer: the receive process IS running and hears
        // nobody. That is a real answer, not an absence.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SIDECAR_FILE);
        let now = 1_700_000_000.0;
        std::fs::write(
            &path,
            json!({"version": 1, "wall_time_unix": now, "peers": []}).to_string(),
        )
        .unwrap();

        let (status, body) = body_of(read_status(&path, now)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["peer_count"], 0);
        assert!(body["peers"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_peer_with_no_timestamps_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SIDECAR_FILE);
        let now = 1_700_000_000.0;
        std::fs::write(
            &path,
            json!({
                "version": 1,
                "wall_time_unix": now,
                "peers": [{"device_id": "ghost", "status": {"cpu_percent": 1.0}}],
            })
            .to_string(),
        )
        .unwrap();

        let (status, body) = body_of(read_status(&path, now)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["peer_count"], 0,
            "an unageable entry cannot be current"
        );
    }

    #[tokio::test]
    async fn an_identity_only_peer_is_served_without_a_status_block() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SIDECAR_FILE);
        let now = 1_700_000_000.0;
        std::fs::write(
            &path,
            json!({
                "version": 1,
                "wall_time_unix": now,
                "peers": [{
                    "device_id": "drone-a",
                    "name": "Alpha",
                    "identity_at_unix": now - 3.0,
                }],
            })
            .to_string(),
        )
        .unwrap();

        let (status, body) = body_of(read_status(&path, now)).await;
        assert_eq!(status, StatusCode::OK);
        let p = &body["peers"][0];
        assert_eq!(p["name"], "Alpha");
        assert_eq!(p["status_fresh"], false);
        assert_eq!(p["identity_age_s"], 3.0);
        assert!(p.get("status").is_none());
    }

    #[tokio::test]
    async fn an_older_sidecar_without_a_published_window_falls_back() {
        // Compatibility: a receive process predating the published window still
        // gets its peers aged, using the default rather than no gate at all.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SIDECAR_FILE);
        let now = 1_700_000_000.0;
        std::fs::write(
            &path,
            json!({
                "version": 1,
                "wall_time_unix": now,
                "peers": [{
                    "device_id": "drone-a",
                    "status_at_unix": now - (DEFAULT_STATUS_STALE_AFTER_S + 1.0),
                    "status": {"cpu_percent": 9.0},
                }],
            })
            .to_string(),
        )
        .unwrap();

        let (_, body) = body_of(read_status(&path, now)).await;
        assert_eq!(body["status_stale_after_s"], DEFAULT_STATUS_STALE_AFTER_S);
        assert_eq!(body["peers"][0]["status_fresh"], false);
        assert!(body["peers"][0].get("status").is_none());
    }
}
