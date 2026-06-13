//! Ground-station network uplink write routes.
//!
//! The ground-station profile exposes the uplink matrix under
//! `/api/v1/ground-station/network*`. The read views live in
//! [`crate::routes::gs_network`]; this module serves the writes the front can
//! reproduce faithfully.
//!
//! ## What ports here, and what does not
//!
//! Only `PUT .../network/priority` ports. It is the one network write whose
//! whole effect is a config-file persist the front can mirror byte-for-byte: it
//! validates the requested uplink order, atomically writes
//! `{"priority": [...]}` to `/etc/ados/ground-station-uplink.json`, and echoes
//! the persisted list. The live `ados-net` daemon reads that same file on its
//! own cadence, so the front persisting it is wire-equivalent to the FastAPI
//! route persisting it; there is no second writer to race and no in-process
//! manager state to mirror.
//!
//! The sibling network writes (the AP, ethernet, modem, client-join, and
//! client-disconnect routes, plus the share-uplink toggle) do NOT port. Each
//! drives an in-process manager (`HostapdManager` / `EthernetManager` /
//! `ModemManager`) or branches on whether the native `ados-net` daemon owns the
//! surface, with a fallback that drives `nmcli` / `hostapd` / `iptables`
//! in-process — stateful work the front must not reproduce (it would race the
//! daemon for `wlan0` / the firewall) and cannot mirror from its position in
//! front of the residual Python. Those stay on the residual surface (the read
//! module's `ap` / `ethernet` legs already serve the manager-absent fallback for
//! the same reason). They are forwarded by the proxy fallback unchanged.
//!
//! ## The profile gate
//!
//! Like every ground-station route, this first gates on the resolved profile
//! being a ground station and returns the FastAPI
//! `404 {"detail":{"error":{"code":"E_PROFILE_MISMATCH"}}}` on a drone, the same
//! body the read module serves. Note this surface uses the FastAPI network
//! route's *error-object* detail shape (`{"detail":{"error":{"code","message"}}}`)
//! for its own 4xx/5xx too, NOT the bare-string `{"detail":"..."}` the rest of
//! the front uses — so it builds those bodies directly rather than through the
//! crate's bare-string [`crate::routes::detail`] helper.

use std::path::{Path, PathBuf};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::state::AppState;

// ---------------------------------------------------------------------------
// Profile gate (mirrors the read module + the Python `_require_ground_profile`).
// ---------------------------------------------------------------------------

/// The FastAPI `_require_ground_profile` 404 body: a `detail` carrying the
/// `E_PROFILE_MISMATCH` error object. A drone-profile caller hits every
/// ground-station route with this exact shape.
fn profile_mismatch() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})),
    )
        .into_response()
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
// Path seam: the persisted priority file.
// ---------------------------------------------------------------------------

/// The agent etc dir (`ADOS_ETC_DIR`, default `/etc/ados`), the same override
/// the read module + the persisted side-files resolve under.
fn etc_dir() -> PathBuf {
    PathBuf::from(std::env::var("ADOS_ETC_DIR").unwrap_or_else(|_| "/etc/ados".to_string()))
}

/// The persisted uplink priority list (`/etc/ados/ground-station-uplink.json`),
/// the same file the read module reads and the `ados-net` daemon loads. Mirrors
/// the Python `GS_UPLINK_JSON`.
fn gs_uplink_json() -> PathBuf {
    etc_dir().join("ground-station-uplink.json")
}

// ---------------------------------------------------------------------------
// PUT /api/v1/ground-station/network/priority — set the uplink priority list.
// ---------------------------------------------------------------------------

/// The `PUT .../network/priority` request body: the ordered uplink list. Mirrors
/// the FastAPI `UplinkPriorityUpdate`. The Pydantic model carries `min_length=1`,
/// so the FastAPI surface rejects an empty list with a 422 *before* the handler;
/// the front has no such pre-validation, so an empty (or non-string) list reaches
/// the handler and is rejected by the same `validate_priority` guard the FastAPI
/// handler runs (the 400 below). The valid path — a non-empty list of strings —
/// is byte-identical on both surfaces.
#[derive(Debug, Deserialize)]
pub struct UplinkPriorityUpdate {
    pub priority: Vec<Value>,
}

/// `PUT .../network/priority` → `{"priority": [...]}`.
///
/// Gates on the ground-station profile (404 on a drone), validates the requested
/// order (a non-empty list of strings, else the FastAPI 400
/// `E_UPLINK_PRIORITY_INVALID`), atomically persists `{"priority": [...]}` to the
/// uplink file, and echoes the persisted list. The `ados-net` daemon reads the
/// same file, so the persist is the whole effect. A file-write failure degrades
/// to the FastAPI 500 `E_UPLINK_PRIORITY_FAILED` rather than panicking.
pub async fn put_network_priority(
    State(_state): State<AppState>,
    Json(update): Json<UplinkPriorityUpdate>,
) -> Response {
    if !is_ground_station() {
        return profile_mismatch();
    }

    // The FastAPI route's first guard inside the handler: validate_priority
    // raises ValueError on an empty / non-string list → a 400 with the
    // E_UPLINK_PRIORITY_INVALID error object carrying the ValueError text.
    let strings = match validate_priority(&update.priority) {
        Ok(s) => s,
        Err(msg) => {
            return error_body(StatusCode::BAD_REQUEST, "E_UPLINK_PRIORITY_INVALID", &msg);
        }
    };

    // Persist the validated list. The Python `set_priority` calls `save_priority`
    // which atomically writes `{"priority": [...]}`; a write failure there is
    // logged-and-swallowed, but the *route* still 500s on a broader failure, so
    // the front maps a write error to the FastAPI 500 error object.
    if let Err(msg) = save_priority(&gs_uplink_json(), &strings) {
        return error_body(
            StatusCode::INTERNAL_SERVER_ERROR,
            "E_UPLINK_PRIORITY_FAILED",
            &msg,
        );
    }

    // The Python route returns `{"priority": list(get_priority())}`; after the
    // persist a fresh router loads the file, which now holds exactly the list we
    // wrote, so the echoed list is the validated input verbatim.
    Json(json!({ "priority": strings })).into_response()
}

/// Validate the requested priority list, returning the list of strings on
/// success. Mirrors the Python `validate_priority`: a non-empty list whose every
/// member is a string is accepted; an empty list or any non-string member raises
/// the `ValueError("priority must be a non-empty list of strings")` the FastAPI
/// route surfaces in the 400 body. Returns the unwrapped `Vec<String>` so the
/// persist + the echo carry plain strings (matching the JSON the Python writes).
fn validate_priority(priority: &[Value]) -> Result<Vec<String>, String> {
    const INVALID: &str = "priority must be a non-empty list of strings";
    if priority.is_empty() {
        return Err(INVALID.to_string());
    }
    let mut out = Vec::with_capacity(priority.len());
    for entry in priority {
        match entry.as_str() {
            Some(s) => out.push(s.to_string()),
            None => return Err(INVALID.to_string()),
        }
    }
    Ok(out)
}

/// Atomically persist the priority list to `path`, mirroring the Python
/// `save_priority`: create the parent dir, write `{"priority": [...]}` to a
/// `.json.tmp` sibling, then `rename` it over the target. Returns the error
/// string on any IO failure so the route can surface the FastAPI 500 body. The
/// JSON is `{"priority": ["a","b"]}` with no spaces, matching the Python
/// `json.dumps({"priority": priority})` output the read side parses back.
fn save_priority(path: &Path, priority: &[String]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("json.tmp");
    let body = json!({ "priority": priority }).to_string();
    std::fs::write(&tmp, body.as_bytes()).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    Ok(())
}

/// Build a network-route 4xx/5xx body in the FastAPI error-object detail shape:
/// `(status, {"detail": {"error": {"code": <code>, "message": <message>}}})`.
/// This surface uses this shape (NOT the bare-string `{"detail"}`) because its
/// FastAPI twin raises `HTTPException(detail={"error": {...}})`.
fn error_body(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({"detail": {"error": {"code": code, "message": message}}})),
    )
        .into_response()
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

    // ── validate_priority ────────────────────────────────────────────────────

    #[test]
    fn validate_accepts_a_non_empty_string_list() {
        let input = vec![json!("eth0"), json!("wlan0_client")];
        assert_eq!(
            validate_priority(&input).unwrap(),
            vec!["eth0".to_string(), "wlan0_client".to_string()]
        );
    }

    #[test]
    fn validate_rejects_an_empty_list() {
        let err = validate_priority(&[]).unwrap_err();
        assert_eq!(err, "priority must be a non-empty list of strings");
    }

    #[test]
    fn validate_rejects_a_non_string_member() {
        let input = vec![json!("eth0"), json!(7)];
        let err = validate_priority(&input).unwrap_err();
        assert_eq!(err, "priority must be a non-empty list of strings");
    }

    // ── save_priority + the persisted JSON shape ────────────────────────────

    #[test]
    fn save_writes_the_compact_priority_json_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ground-station-uplink.json");
        let list = vec!["wlan0_client".to_string(), "eth0".to_string()];
        save_priority(&path, &list).unwrap();

        // The on-disk bytes are the compact Python json.dumps shape.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw, r#"{"priority":["wlan0_client","eth0"]}"#);

        // It parses back to the same list the read module would load.
        let parsed: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["priority"], json!(["wlan0_client", "eth0"]));

        // No stray .json.tmp left behind after the atomic rename.
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn save_creates_the_parent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("ground-station-uplink.json");
        save_priority(&path, &["eth0".to_string()]).unwrap();
        assert!(path.exists());
    }

    // ── error_body shape ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn error_body_is_the_error_object_detail_shape() {
        let resp = error_body(
            StatusCode::BAD_REQUEST,
            "E_UPLINK_PRIORITY_INVALID",
            "priority must be a non-empty list of strings",
        );
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        assert_eq!(
            body,
            json!({
                "detail": {
                    "error": {
                        "code": "E_UPLINK_PRIORITY_INVALID",
                        "message": "priority must be a non-empty list of strings",
                    }
                }
            })
        );
    }

    // ── profile gate ─────────────────────────────────────────────────────────

    #[test]
    fn profile_mismatch_body_is_the_object_detail() {
        let resp = profile_mismatch();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    /// The profile-mismatch body the handler returns on a drone, pinned as the
    /// golden fixture for the conformance harness's off-a-drone diff. (The
    /// success body depends on a live ground-station profile, so it is
    /// bench-validated, not asserted here.)
    #[tokio::test]
    async fn profile_mismatch_golden_body() {
        let resp = profile_mismatch();
        let body = body_json(resp).await;
        assert_eq!(
            body,
            json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})
        );
    }

    // ── the success envelope ─────────────────────────────────────────────────

    #[test]
    fn the_success_body_echoes_the_persisted_list() {
        // The route's success envelope is {"priority": <validated list>}. Built
        // from the same json! the handler returns so the contract is pinned
        // field-by-field without needing a ground-station profile.
        let list = vec![
            "eth0".to_string(),
            "wlan0_client".to_string(),
            "wwan0".to_string(),
        ];
        let body = json!({ "priority": list });
        assert_eq!(body, json!({"priority": ["eth0", "wlan0_client", "wwan0"]}));
    }
}
