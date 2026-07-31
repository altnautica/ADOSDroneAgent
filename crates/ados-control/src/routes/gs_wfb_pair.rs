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
//! ## The fleet gate
//!
//! `POST .../wfb/pair` runs the guards in order: profile gate (404
//! `E_PROFILE_MISMATCH`); the deprecated-`pair_key` 400; the missing-`blob_b64`
//! 400; the missing-`drone_device_id` 400; then the FLEET gate.
//!
//! A fleet of up to [`FLEET_MAX_SLOTS`] drones shares ONE keypair — the wfb-ng
//! `channel_id` separates the drones, not the key — so a second drone presenting
//! the same blob is a normal fleet join. The gate compares BYTES: identical
//! accepts (and skips the re-install, which would restart the receive unit and
//! blip every drone's video), different is 409 `E_FLEET_KEY_MISMATCH`, and a
//! registry with no free slot is 409 `E_FLEET_FULL`. On acceptance a slot is
//! allocated from the persisted [`FleetRegistry`] — idempotent by device id, so
//! a re-pair never renumbers a drone that may be airborne — and returned as
//! `fleet_slot`, alongside the whole `slots` table.
//!
//! A fresh install is still forwarded to the command socket, whose
//! `pair_keypair` op decodes + validates the blob (a base64 fault → 400
//! `E_BLOB_BASE64`, a wrong length → 400 `E_INVALID_KEY_BLOB`, an IO fault →
//! 500 `E_PAIR_FAILED`) and returns the
//! `{paired,paired_with_device_id,paired_at,fingerprint,role}` body this route
//! extends. `DELETE .../wfb/pair` forwards the `unpair` op and returns
//! `{paired:false, role:"gs"}`.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use ados_groundlink::{FleetRegistry, FLEET_MAX_SLOTS, FLEET_REGISTRY_PATH};

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
// GS pair-status read (the slot table + the single-peer keys).
// ---------------------------------------------------------------------------

/// The ground station's pair status.
///
/// Whether a fleet key is installed: the rx.key exists, is exactly 64 bytes,
/// and yields a readable fingerprint. The fleet's composition is the registry,
/// read where it is actually returned rather than carried here.
struct GsPairStatus {
    paired: bool,
}

/// Read the GS pair status. Mirrors the bits of `PairManager.status("gs")` the
/// FastAPI route consulted. A status read fault is
/// treated as not-paired (the FastAPI route's
/// `except Exception: current = {"paired": False}`); an unreadable registry is
/// an empty slot table, never a failure.
fn gs_pair_status() -> GsPairStatus {
    let key = rx_key_path();
    let mut paired = std::fs::metadata(&key)
        .map(|m| m.is_file() && m.len() == WFB_KEY_FILE_BYTES)
        .unwrap_or(false);
    if paired && read_public_fingerprint(&key).is_none() {
        // A 64-byte file whose fingerprint cannot be read reverts paired to false,
        // matching the Python `except (OSError, ValueError): paired = False`.
        paired = false;
    }
    GsPairStatus { paired }
}

/// Load the fleet registry from its canonical path. A missing or unparseable
/// file is an empty fleet — `FleetRegistry::load` already has that contract.
fn load_registry() -> FleetRegistry {
    FleetRegistry::load(std::path::Path::new(FLEET_REGISTRY_PATH))
}

/// Render the registry as the `slots` array the route returns, in slot order.
fn slot_table(registry: &FleetRegistry) -> Vec<Value> {
    registry
        .slots()
        .map(|s| {
            json!({
                "slot": s.slot,
                "device_id": s.device_id,
                "paired_at_ms": s.paired_at_ms,
            })
        })
        .collect()
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

/// `POST .../wfb/pair` →
/// `{paired,paired_with_device_id,paired_at,fingerprint,role,fleet_slot,slots}`.
///
/// Guards in order: profile, deprecated-`pair_key`, missing-blob, blob decode,
/// then the FLEET gate.
///
/// A fleet is one trust domain sharing one keypair — `channel_id` separates the
/// drones, not the key — so a second drone presenting the SAME blob is a normal
/// fleet join, not a conflict. The gate is therefore on the bytes, not on
/// presence:
///
/// * key absent → install it through the command socket's `pair_keypair` op,
///   then allocate a slot;
/// * key present and byte-identical → the fleet key is already installed;
///   allocate a slot and return 200 WITHOUT re-forwarding the install (a
///   re-install stops and restarts the receive unit, blipping every drone's
///   video for a write that changes nothing);
/// * key present and different → 409 `E_FLEET_KEY_MISMATCH`; installing it
///   would deafen every already-paired drone;
/// * all [`FLEET_MAX_SLOTS`] slots taken → 409 `E_FLEET_FULL`.
///
/// A base64 fault is the 400 `E_BLOB_BASE64`, a wrong length the 400
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

    // A slot is issued TO a device and `FleetRegistry::allocate` is idempotent by
    // device id, so without one every re-pair would burn a fresh slot until the
    // fleet reported full. Refuse loudly rather than hand out a slot nothing can
    // be re-matched to.
    let Some(device_id) = req.drone_device_id.filter(|s| !s.is_empty()) else {
        return nested_detail(
            StatusCode::BAD_REQUEST,
            json!({
                "code": "E_DEVICE_ID_REQUIRED",
                "message": "drone_device_id is required: a fleet slot is issued to a device and re-pairing is matched by it",
            }),
        );
    };

    // Decode here as well as in the socket op: the byte-identity gate below
    // compares the presented key against the installed one, and a base64 fault
    // must surface as the same 400 the op would have returned.
    let blob = match base64::engine::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        blob_b64.as_bytes(),
    ) {
        Ok(b) => b,
        Err(e) => {
            return nested_detail(
                StatusCode::BAD_REQUEST,
                json!({"code": "E_BLOB_BASE64", "message": e.to_string()}),
            )
        }
    };

    // Fleet-key gate. `installed` is the on-disk fleet key, if any.
    let status = gs_pair_status();
    let installed = status
        .paired
        .then(|| std::fs::read(rx_key_path()).ok())
        .flatten();
    let mut body = match installed {
        Some(existing) if existing != blob => {
            // A caller presenting a DIFFERENT key has just proved it does not
            // belong to this fleet, so it learns only that the key does not
            // match. This used to answer with the peer device id and the whole
            // slot table — every member's device id, slot and pairing time —
            // handing the fleet's roster to the one caller shown not to hold
            // its key. The successful path still returns the table, because a
            // caller with the right key is in the fleet already.
            return nested_detail(
                StatusCode::CONFLICT,
                json!({
                    "code": "E_FLEET_KEY_MISMATCH",
                    "message": "this ground station already holds a different fleet key; unpair before pairing a different fleet",
                }),
            );
        }
        // Byte-identical: the fleet key is already installed. Skip the install.
        Some(_) => Map::new(),
        None => {
            // Forward the install. The socket's pair_keypair op decodes +
            // validates the blob, writes rx.key + the pair state, drops the
            // sentinel, and restarts the receive unit; its reply carries the
            // install body the FastAPI route returned.
            let request = json!({
                "op": "pair_keypair",
                "blob_b64": blob_b64,
                "peer_device_id": device_id,
            });
            let reply = match groundlink_cmd_roundtrip(&request).await {
                Some(r) => r,
                None => return socket_unavailable("E_PAIR_FAILED"),
            };
            match split_reply(reply) {
                Ok(b) => b,
                Err(err) => return map_pair_error(err),
            }
        }
    };

    // Issue the slot. Idempotent by device id, so a re-pair returns the slot the
    // drone already holds and never renumbers one that may be airborne.
    let mut registry = load_registry();
    let Some(slot) = registry.allocate(&device_id) else {
        return nested_detail(
            StatusCode::CONFLICT,
            json!({
                "code": "E_FLEET_FULL",
                "message": format!("all {FLEET_MAX_SLOTS} fleet slots are taken; release one before pairing another drone"),
                "slots": slot_table(&registry),
            }),
        );
    };
    if let Err(e) = registry.persist(std::path::Path::new(FLEET_REGISTRY_PATH)) {
        // The slot exists only in memory now, so the next pair would re-issue it
        // to a different drone and put two transmitters on one channel_id. Refuse
        // rather than return an assignment the ground station will not honour.
        tracing::error!(error = %e, device_id = %device_id, slot, "fleet_registry_persist_failed");
        return nested_detail(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({
                "code": "E_FLEET_PERSIST_FAILED",
                "message": e.to_string(),
            }),
        );
    }

    // The fleet fields ride on top of the install body so an existing consumer of
    // `{paired, paired_with_device_id, paired_at, fingerprint, role}` is unchanged.
    // On the already-installed path the body starts empty, so fill the same keys.
    if body.is_empty() {
        body.insert("paired".to_string(), json!(true));
        body.insert("paired_with_device_id".to_string(), json!(device_id));
        body.insert("role".to_string(), json!("gs"));
        body.insert(
            "fingerprint".to_string(),
            json!(read_public_fingerprint(&rx_key_path())),
        );
        body.insert("paired_at".to_string(), Value::Null);
    }
    body.insert("fleet_slot".to_string(), json!(slot));
    body.insert("slots".to_string(), json!(slot_table(&registry)));
    Json(Value::Object(body)).into_response()
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
    fn a_foreign_key_is_refused_without_naming_the_fleet() {
        // A caller presenting a DIFFERENT key has just proved it is not part of
        // this fleet. It used to be answered with the peer device id and the
        // whole slot table — every member's device id, slot and pairing time —
        // so the one caller shown not to hold the key learned the roster.
        let body = json!({
            "code": "E_FLEET_KEY_MISMATCH",
            "message": "this ground station already holds a different fleet key; unpair before pairing a different fleet",
        });
        let obj = body.as_object().unwrap();
        assert!(
            !obj.contains_key("slots"),
            "the fleet roster must not ride a refusal"
        );
        assert!(
            !obj.contains_key("paired_with_device_id"),
            "a refused caller must not learn who this station is paired with"
        );
        // It must still say WHY, or the operator cannot act on it.
        assert_eq!(obj["code"], "E_FLEET_KEY_MISMATCH");
        assert!(obj["message"].as_str().unwrap().contains("unpair"));
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

    // ── the fleet gate ────────────────────────────────────────────────────────

    #[test]
    fn the_slot_table_renders_the_registry_in_slot_order() {
        // The GCS reads this table to draw the fleet, and the pair route returns
        // it on both the success and the conflict paths, so its shape is a wire
        // contract.
        let mut registry = FleetRegistry::default();
        registry.allocate("drone-b").unwrap();
        registry.allocate("drone-a").unwrap();
        let table = slot_table(&registry);
        assert_eq!(table.len(), 2);
        assert_eq!(table[0]["slot"], 1);
        assert_eq!(table[0]["device_id"], "drone-b");
        assert!(table[0]["paired_at_ms"].as_u64().unwrap() > 0);
        assert_eq!(table[1]["slot"], 2);
        assert_eq!(table[1]["device_id"], "drone-a");
    }

    #[test]
    fn an_empty_registry_renders_an_empty_table_not_null() {
        // The GCS iterates this; a null would need a second code path.
        assert_eq!(slot_table(&FleetRegistry::default()), Vec::<Value>::new());
    }

    #[test]
    fn a_fleet_join_is_idempotent_and_a_full_fleet_refuses() {
        // The two registry outcomes the route branches on. Re-pairing the same
        // device must return its existing slot (never renumber a flying drone),
        // and a full fleet must refuse a NEW device while still serving a known
        // one — the E_FLEET_FULL branch must not fire for a re-pair.
        let mut registry = FleetRegistry::default();
        let first = registry.allocate("drone-a").unwrap();
        assert_eq!(registry.allocate("drone-a"), Some(first));
        for i in 2..=FLEET_MAX_SLOTS {
            assert!(registry.allocate(&format!("drone-{i}")).is_some());
        }
        assert_eq!(registry.allocate("one-too-many"), None);
        assert_eq!(registry.allocate("drone-a"), Some(first));
    }

    #[tokio::test]
    async fn a_missing_device_id_is_refused_before_anything_is_installed() {
        // A slot is issued TO a device and allocation is idempotent by device id.
        // Without one, every re-pair would burn a fresh slot until the fleet
        // reported full, so the route refuses rather than issuing an
        // unmatchable slot. Drive the guard's body shape directly (the handler
        // needs an AppState + the GS profile sentinel).
        let resp = nested_detail(
            StatusCode::BAD_REQUEST,
            json!({
                "code": "E_DEVICE_ID_REQUIRED",
                "message": "drone_device_id is required: a fleet slot is issued to a device and re-pairing is matched by it",
            }),
        );
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_json(resp).await["detail"]["error"]["code"],
            "E_DEVICE_ID_REQUIRED"
        );
    }

    #[test]
    fn the_key_gate_accepts_an_identical_blob_and_rejects_a_different_one() {
        // The whole fleet model rests on this: one keypair per fleet, with
        // `channel_id` separating the drones. A second drone presenting the SAME
        // key is a join; a DIFFERENT key would deafen every drone already paired,
        // so byte-identity — not mere presence — is the gate.
        let installed = vec![3u8; 64];
        let same = vec![3u8; 64];
        let different = vec![4u8; 64];
        assert_eq!(installed, same, "an identical blob must pass the gate");
        assert_ne!(installed, different, "a different blob must be refused");
    }
}
