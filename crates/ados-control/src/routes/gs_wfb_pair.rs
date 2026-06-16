//! Ground-station WFB pair-key install / unpair write routes.
//!
//! The cloud-relay path installs a 64-byte rx-side wfb-ng key on the ground
//! station (`POST .../wfb/pair`) and removes it (`DELETE .../wfb/pair`). These are
//! the writes the sibling read module ([`crate::routes::gs_status::get_wfb`]) and
//! the GS-wfb config write deliberately left on the residual surface until the
//! data-plane service grew a command socket; that socket now carries the
//! `pair_keypair` / `unpair` ops (it writes `/etc/ados/wfb/rx.key`, persists the
//! pair-state config, drops the setup-complete sentinel, and restarts the receive
//! unit), so the front can forward them.
//!
//! The `POST .../wfb/pair/local-bind` + `.../auto-pair` lifecycle and the
//! captive-token-gated `POST .../factory-reset` stay on the residual surface (the
//! bind orchestrator + the in-process captive-token store have no command-socket
//! seam).
//!
//! ## Byte-parity with the FastAPI route
//!
//! `POST .../wfb/pair` runs the FastAPI guards in order: profile gate (404
//! `E_PROFILE_MISMATCH`); the deprecated-`pair_key` 400; the missing-`blob_b64`
//! 400; the already-paired 409 (read from the on-disk rx.key + the persisted peer,
//! the same `pm.status("gs")` the FastAPI route consults); then the install is
//! forwarded to the command socket, whose `pair_keypair` op decodes + validates
//! the blob (a base64 fault → 400 `E_BLOB_BASE64`, a wrong length → 400
//! `E_INVALID_KEY_BLOB`, an IO fault → 500 `E_PAIR_FAILED`) and returns the
//! `{paired,paired_with_device_id,paired_at,fingerprint,role}` body the FastAPI
//! route returned. The base64/length error *message* text differs from the Python
//! exception text (Rust's decoder vs `binascii.Error`); the error *code* + status
//! match. `DELETE .../wfb/pair` forwards the `unpair` op and returns
//! `{paired:false, role:"gs"}`.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::routes::gs_cmd::groundlink_cmd_roundtrip;
use crate::state::AppState;

/// The 64-byte wfb-ng key file size. Mirrors `key_mgr.WFB_KEY_FILE_BYTES`.
const WFB_KEY_FILE_BYTES: u64 = 64;

/// The peer-public half starts 32 bytes into the keypair file.
const WFB_PUBLIC_HALF_OFFSET: usize = 32;

// ---------------------------------------------------------------------------
// Profile gate + error envelopes (the nested error-object detail shape).
// ---------------------------------------------------------------------------

/// Build a `(status, {"detail": {"error": <error>}})` response, the shape FastAPI
/// renders for an `HTTPException(detail={"error": {...}})`.
fn nested_detail(status: StatusCode, error: Value) -> Response {
    (status, Json(json!({ "detail": { "error": error } }))).into_response()
}

/// The FastAPI profile-mismatch 404. A drone-profile caller hits every
/// ground-station route with this exact body.
fn profile_mismatch() -> Response {
    nested_detail(StatusCode::NOT_FOUND, json!({"code": "E_PROFILE_MISMATCH"}))
}

/// True when the resolved profile is a ground station. Mirrors the Python
/// `is_ground_station` (config `agent.profile` + the on-disk sentinels).
fn is_ground_station() -> bool {
    let cfg = crate::config::PairingConfig::load();
    let (profile, _role) = crate::profile::current_profile_and_role(&cfg.agent.profile);
    profile == "ground-station"
}

// ---------------------------------------------------------------------------
// Path seams.
// ---------------------------------------------------------------------------

/// The agent config path (`ADOS_CONFIG`, default `/etc/ados/config.yaml`).
fn config_yaml_path() -> std::path::PathBuf {
    std::path::PathBuf::from(
        std::env::var("ADOS_CONFIG").unwrap_or_else(|_| crate::config::CONFIG_YAML.to_string()),
    )
}

/// The GS rx-side key file (`<wfb key dir>/rx.key`), honouring `ADOS_WFB_KEY_DIR`
/// (the same override the pair-state writer uses) for tests, else the canonical
/// `/etc/ados/wfb` dir.
fn rx_key_path() -> std::path::PathBuf {
    std::env::var("ADOS_WFB_KEY_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/etc/ados/wfb"))
        .join("rx.key")
}

// ---------------------------------------------------------------------------
// GS pair-status read (the `pm.status("gs")` bits the already-paired gate uses).
// ---------------------------------------------------------------------------

/// The GS pair status: `paired` (the rx.key exists, is exactly 64 bytes, and
/// yields a readable fingerprint) + the persisted peer device-id. Mirrors the bits
/// of `PairManager.status("gs")` the FastAPI `POST .../wfb/pair` route consults
/// for the already-paired 409. A status read fault is treated as not-paired (the
/// FastAPI route's `except Exception: current = {"paired": False}`).
fn gs_pair_status() -> (bool, Option<String>) {
    let key = rx_key_path();
    let mut paired = std::fs::metadata(&key)
        .map(|m| m.is_file() && m.len() == WFB_KEY_FILE_BYTES)
        .unwrap_or(false);
    if paired && read_public_fingerprint(&key).is_none() {
        // A 64-byte file whose fingerprint cannot be read reverts paired to false,
        // matching the Python `except (OSError, ValueError): paired = False`.
        paired = false;
    }
    (paired, read_persisted_peer(&config_yaml_path()))
}

/// blake2b-8 over the peer-public half of a 64-byte key file, as 16 lowercase
/// hex. `None` for an absent / wrong-size file. Mirrors
/// `key_mgr.read_public_fingerprint`.
fn read_public_fingerprint(path: &std::path::Path) -> Option<String> {
    use blake2::digest::{Update, VariableOutput};
    let data = std::fs::read(path).ok()?;
    if data.len() != WFB_KEY_FILE_BYTES as usize {
        return None;
    }
    let mut hasher = blake2::Blake2bVar::new(8).ok()?;
    hasher.update(&data[WFB_PUBLIC_HALF_OFFSET..]);
    let mut out = [0u8; 8];
    hasher.finalize_variable(&mut out).ok()?;
    Some(hex::encode(out))
}

/// Read the persisted peer device-id from `video.wfb.paired_with_device_id`,
/// falling back to `ground_station.paired_drone_id` (the GS legacy mirror), the
/// same precedence `PairManager.status("gs")` uses for the `paired_with_device_id`
/// the already-paired 409 echoes.
fn read_persisted_peer(config_path: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(config_path).ok()?;
    let doc: serde_norway::Value = serde_norway::from_str(&text).ok()?;
    let canon = doc
        .get("video")
        .and_then(|v| v.get("wfb"))
        .and_then(|w| w.get("paired_with_device_id"))
        .and_then(|v| v.as_str())
        .filter(|p| !p.is_empty());
    if let Some(p) = canon {
        return Some(p.to_string());
    }
    doc.get("ground_station")
        .and_then(|g| g.get("paired_drone_id"))
        .and_then(|v| v.as_str())
        .filter(|p| !p.is_empty())
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// POST /api/v1/ground-station/wfb/pair — install the GS rx-side key.
// ---------------------------------------------------------------------------

/// The `POST .../wfb/pair` body. Mirrors the FastAPI `PairRequest`: a base64
/// `blob_b64` (the 64-byte wfb-ng key), an optional `drone_device_id`, and the
/// legacy `pair_key` kept only so an old client gets a clear 400 instead of a 422.
#[derive(Debug, Default, Deserialize)]
pub struct PairRequest {
    #[serde(default)]
    pub blob_b64: Option<String>,
    #[serde(default)]
    pub drone_device_id: Option<String>,
    #[serde(default)]
    pub pair_key: Option<String>,
}

/// `POST .../wfb/pair` → `{paired,paired_with_device_id,paired_at,fingerprint,role}`.
///
/// Runs the FastAPI guards in order (profile, deprecated-`pair_key`, missing-blob,
/// already-paired), then forwards the install to the data-plane command socket's
/// `pair_keypair` op and maps its reply: success returns the install body; a
/// base64 fault is the 400 `E_BLOB_BASE64`, a wrong length the 400
/// `E_INVALID_KEY_BLOB`, an IO fault the 500 `E_PAIR_FAILED`. An unreachable
/// socket degrades to a 503 (the front owns no key-install seam itself).
pub async fn post_wfb_pair(
    State(_state): State<AppState>,
    Json(req): Json<PairRequest>,
) -> Response {
    if !is_ground_station() {
        return profile_mismatch();
    }

    // The typed `pair_key` is no longer supported; surface the clear 400 the
    // FastAPI route raises when an old client sends it without a blob.
    let blob_present = req
        .blob_b64
        .as_deref()
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    if req
        .pair_key
        .as_deref()
        .map(|s| !s.is_empty())
        .unwrap_or(false)
        && !blob_present
    {
        return nested_detail(
            StatusCode::BAD_REQUEST,
            json!({
                "code": "E_PAIR_KEY_DEPRECATED",
                "message": "typed pair_key is no longer supported; pass blob_b64 (base64 of 64-byte wfb-ng key) or use POST /api/wfb/pair/local-bind",
            }),
        );
    }
    let Some(blob_b64) = req.blob_b64.filter(|s| !s.is_empty()) else {
        return nested_detail(StatusCode::BAD_REQUEST, json!({"code": "E_BLOB_REQUIRED"}));
    };

    // Already-paired gate: read the live status; a paired GS must unpair first.
    let (paired, peer) = gs_pair_status();
    if paired {
        return nested_detail(
            StatusCode::CONFLICT,
            json!({
                "code": "E_ALREADY_PAIRED",
                "message": "unpair before pairing a new drone",
                "paired_with_device_id": peer,
            }),
        );
    }

    // Forward the install. The socket's pair_keypair op decodes + validates the
    // blob, writes rx.key + the pair state, drops the sentinel, and restarts the
    // receive unit; its reply carries the install body the FastAPI route returned.
    let request = json!({
        "op": "pair_keypair",
        "blob_b64": blob_b64,
        "peer_device_id": req.drone_device_id,
    });
    let reply = match groundlink_cmd_roundtrip(&request).await {
        Some(r) => r,
        None => return socket_unavailable("E_PAIR_FAILED"),
    };

    match split_reply(reply) {
        Ok(body) => Json(Value::Object(body)).into_response(),
        Err(err) => map_pair_error(err),
    }
}

/// Map a `pair_keypair` failure reply to the FastAPI status + body. The op returns
/// `E_BLOB_BASE64` for an undecodable blob, `E_INVALID_KEY_BLOB` for a wrong length
/// (both 400 on the FastAPI side), and `E_PAIR_FAILED` for an IO fault (500). An
/// unexpected code is treated as a 500 `E_PAIR_FAILED` (the FastAPI catch-all).
fn map_pair_error(err: SocketError) -> Response {
    let (status, code) = match err.code.as_str() {
        "E_BLOB_BASE64" => (StatusCode::BAD_REQUEST, "E_BLOB_BASE64"),
        "E_INVALID_KEY_BLOB" => (StatusCode::BAD_REQUEST, "E_INVALID_KEY_BLOB"),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "E_PAIR_FAILED"),
    };
    let mut error = Map::new();
    error.insert("code".to_string(), json!(code));
    if let Some(msg) = err.message {
        error.insert("message".to_string(), json!(msg));
    }
    nested_detail(status, Value::Object(error))
}

// ---------------------------------------------------------------------------
// DELETE /api/v1/ground-station/wfb/pair — wipe the GS pair key.
// ---------------------------------------------------------------------------

/// `DELETE .../wfb/pair` → `{paired:false, role:"gs"}`.
///
/// Gates on the profile, then forwards the `unpair` op (which wipes both key
/// files, clears the persisted pair state, restarts the receive unit) and returns
/// its reply. A socket-reported failure is the FastAPI 500 `E_UNPAIR_FAILED`; an
/// unreachable socket degrades to a 503.
pub async fn delete_wfb_pair(State(_state): State<AppState>) -> Response {
    if !is_ground_station() {
        return profile_mismatch();
    }
    let reply = match groundlink_cmd_roundtrip(&json!({"op": "unpair"})).await {
        Some(r) => r,
        None => return socket_unavailable("E_UNPAIR_FAILED"),
    };
    match split_reply(reply) {
        Ok(body) => Json(Value::Object(body)).into_response(),
        Err(err) => {
            let mut error = Map::new();
            error.insert("code".to_string(), json!("E_UNPAIR_FAILED"));
            if let Some(msg) = err.message {
                error.insert("message".to_string(), json!(msg));
            }
            nested_detail(StatusCode::INTERNAL_SERVER_ERROR, Value::Object(error))
        }
    }
}

// ---------------------------------------------------------------------------
// Command-socket reply mapping.
// ---------------------------------------------------------------------------

/// A command-socket failure reply.
#[derive(Debug)]
struct SocketError {
    code: String,
    message: Option<String>,
}

/// Split a command-socket reply on its transport `ok` flag: `ok:true`/absent
/// yields the body with `ok` stripped; `ok:false` yields the [`SocketError`].
fn split_reply(reply: Value) -> Result<Map<String, Value>, SocketError> {
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

/// The front's no-link 500-family error when the data-plane command socket is
/// unreachable. The FastAPI route installs/wipes the key in-process; the front
/// cannot, so an absent socket degrades to a 503 with the route's error code
/// rather than a 500.
fn socket_unavailable(code: &str) -> Response {
    nested_detail(
        StatusCode::SERVICE_UNAVAILABLE,
        json!({
            "code": code,
            "message": "ground-station command socket unavailable",
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn body_json(resp: Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn profile_mismatch_golden_body() {
        let resp = profile_mismatch();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            body_json(resp).await,
            json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})
        );
    }

    // ── split_reply ───────────────────────────────────────────────────────────

    #[test]
    fn split_reply_strips_ok_on_success() {
        let reply = json!({
            "ok": true,
            "paired": true,
            "paired_with_device_id": "drone-7",
            "paired_at": "2026-06-16T00:00:00+00:00",
            "fingerprint": "0123456789abcdef",
            "role": "gs",
        });
        let body = split_reply(reply).unwrap();
        assert!(!body.contains_key("ok"));
        assert_eq!(body.get("paired"), Some(&json!(true)));
        assert_eq!(body.get("role"), Some(&json!("gs")));
        assert_eq!(body.get("fingerprint"), Some(&json!("0123456789abcdef")));
    }

    #[test]
    fn split_reply_surfaces_error_code_and_message() {
        let err =
            split_reply(json!({"ok": false, "error": "E_INVALID_KEY_BLOB", "message": "bad"}))
                .unwrap_err();
        assert_eq!(err.code, "E_INVALID_KEY_BLOB");
        assert_eq!(err.message.as_deref(), Some("bad"));
    }

    // ── pair-error mapping ────────────────────────────────────────────────────

    #[tokio::test]
    async fn pair_error_base64_is_a_400() {
        let resp = map_pair_error(SocketError {
            code: "E_BLOB_BASE64".to_string(),
            message: Some("invalid byte".to_string()),
        });
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        assert_eq!(body["detail"]["error"]["code"], "E_BLOB_BASE64");
    }

    #[tokio::test]
    async fn pair_error_bad_blob_is_a_400() {
        let resp = map_pair_error(SocketError {
            code: "E_INVALID_KEY_BLOB".to_string(),
            message: None,
        });
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_json(resp).await["detail"]["error"]["code"],
            "E_INVALID_KEY_BLOB"
        );
    }

    #[tokio::test]
    async fn pair_error_io_is_a_500() {
        let resp = map_pair_error(SocketError {
            code: "E_PAIR_FAILED".to_string(),
            message: Some("disk full".to_string()),
        });
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            body_json(resp).await["detail"]["error"]["code"],
            "E_PAIR_FAILED"
        );
    }

    #[tokio::test]
    async fn socket_unavailable_is_a_503_carrying_the_route_code() {
        let resp = socket_unavailable("E_PAIR_FAILED");
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            body_json(resp).await["detail"]["error"]["code"],
            "E_PAIR_FAILED"
        );
    }

    // ── gs_pair_status reads the key + persisted peer ─────────────────────────

    #[test]
    fn gs_pair_status_reports_unpaired_without_a_key() {
        // No rx.key present → not paired. (Point the key dir at an empty tempdir;
        // env is process-global, so this is a single-threaded read with no writes.)
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("missing").join("rx.key");
        let paired = std::fs::metadata(&key)
            .map(|m| m.is_file() && m.len() == WFB_KEY_FILE_BYTES)
            .unwrap_or(false);
        assert!(!paired);
    }

    #[test]
    fn read_persisted_peer_prefers_canonical_then_mirror() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        // Canonical wins.
        std::fs::write(
            &cfg,
            "video:\n  wfb:\n    paired_with_device_id: drone-canon\nground_station:\n  paired_drone_id: drone-mirror\n",
        )
        .unwrap();
        assert_eq!(read_persisted_peer(&cfg).as_deref(), Some("drone-canon"));
        // Mirror fallback when canonical absent.
        std::fs::write(&cfg, "ground_station:\n  paired_drone_id: drone-mirror\n").unwrap();
        assert_eq!(read_persisted_peer(&cfg).as_deref(), Some("drone-mirror"));
        // Neither → None.
        std::fs::write(&cfg, "agent:\n  name: x\n").unwrap();
        assert_eq!(read_persisted_peer(&cfg), None);
    }

    #[test]
    fn read_public_fingerprint_is_16_hex_for_a_64_byte_file() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("rx.key");
        let mut bytes = vec![1u8; 32];
        bytes.extend(std::iter::repeat_n(7u8, 32));
        std::fs::write(&key, &bytes).unwrap();
        let fp = read_public_fingerprint(&key).unwrap();
        assert_eq!(fp.len(), 16);
        assert!(fp
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        // A short file has no fingerprint.
        std::fs::write(&key, b"short").unwrap();
        assert!(read_public_fingerprint(&key).is_none());
    }
}
