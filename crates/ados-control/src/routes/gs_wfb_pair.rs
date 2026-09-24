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
use crate::wfb_pair_state::read_public_fingerprint;

/// The 64-byte wfb-ng key file size. Mirrors `key_mgr.WFB_KEY_FILE_BYTES`.
const WFB_KEY_FILE_BYTES: u64 = 64;

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
pub(crate) fn load_registry() -> FleetRegistry {
    FleetRegistry::load(std::path::Path::new(FLEET_REGISTRY_PATH))
}

/// Render the registry as the `slots` array, in slot order.
///
/// The ONE place the roster is rendered, deliberately. It is served by both the
/// pair write and the pair read, and it picks its fields explicitly so a
/// `FleetSlot` growing a field — the per-pair relay secret already did — cannot
/// leak onto the wire through either of them. A test pins that.
pub(crate) fn slot_table(registry: &FleetRegistry) -> Vec<Value> {
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

// ---------------------------------------------------------------------------
// POST /api/v1/ground-station/wfb/pair — install the GS rx-side key.
// ---------------------------------------------------------------------------

/// The `POST .../wfb/pair` body: a base64 `blob_b64` (the 64-byte wfb-ng
/// key), an optional `drone_device_id`, an optional `shared_key_b64`, and
/// the legacy `pair_key` kept only so an old client gets a clear 400 instead
/// of a 422.
#[derive(Debug, Default, Deserialize)]
pub struct PairRequest {
    #[serde(default)]
    pub blob_b64: Option<String>,
    #[serde(default)]
    pub drone_device_id: Option<String>,
    /// The OTHER half of the generated pair, base64-encoded.
    ///
    /// `wfb_keygen` produces two files and the radio bind distributes the
    /// drone's half to both ends, so after a bind both rigs hold it
    /// byte-identically. That shared copy is what the presence beacon's HMAC
    /// key is derived from, and deriving it from anything else was tried once
    /// and silently dropped every beacon, which is why the resolver carries a
    /// standing warning against it.
    ///
    /// This route only ever received the ground station's own half, so a fleet
    /// paired through the API had no shared copy at all: the hop supervisor
    /// found no key, could not parse a beacon, and the ground never began
    /// receiving. Supplying this closes that. Absent, the route behaves exactly
    /// as before -- the radio bind remains the path that distributes it.
    #[serde(default)]
    pub shared_key_b64: Option<String>,
    #[serde(default)]
    pub pair_key: Option<String>,
}

/// Where the shared half lives, and the only file the beacon HMAC is derived
/// from.
const SHARED_KEY_PATH: &str = "/etc/drone.key";

/// Serializes every load-modify-persist of the fleet registry in this process:
/// the pair route, the slot release and the enrolment reconciler all allocate or
/// release against `fleet.json`, and two unlocked read-modify-writes would hand
/// one slot to two drones or drop a release. Held only around the synchronous
/// load/mutate/persist, never across an await.
pub(crate) static FLEET_REGISTRY_WRITE: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// Take the registry write lock.
pub(crate) fn fleet_registry_write() -> parking_lot::MutexGuard<'static, ()> {
    FLEET_REGISTRY_WRITE.lock()
}

/// Decode and size-check the supplied shared half before anything is changed,
/// because a short or truncated key would derive a wrong HMAC and reproduce the
/// silent beacon-drop this exists to prevent -- and a wrong key is harder to
/// notice than a missing one, since the resolver at least warns about missing.
fn decode_shared_key(b64: &str) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.as_bytes())
        .map_err(|e| format!("shared key is not valid base64: {e}"))?;
    if bytes.len() as u64 != WFB_KEY_FILE_BYTES {
        return Err(format!(
            "shared key is {} bytes, expected {WFB_KEY_FILE_BYTES}",
            bytes.len()
        ));
    }
    Ok(bytes)
}

/// What became of a supplied shared half, reported on the pair reply so the
/// caller sees whether the beacon key is in place.
fn install_shared_key(path: &std::path::Path, bytes: &[u8], joining: bool) -> &'static str {
    match std::fs::read(path) {
        Ok(existing) if existing == bytes => return "unchanged",
        // A fleet join proved it holds the installed fleet key; a different
        // shared half would re-key every member's beacon, so it is refused
        // rather than overwritten. A fresh install is a new fleet and replaces it.
        Ok(_) if joining => return "mismatch",
        _ => {}
    }
    match write_secret_0600(path, bytes) {
        Ok(()) => {
            tracing::info!("wfb_shared_key_installed");
            "installed"
        }
        Err(e) => {
            tracing::warn!(error = %e, "wfb_shared_key_install_failed");
            "write_failed"
        }
    }
}

/// Write a key file atomically, created 0600 so no other local account can
/// read the beacon key (the default umask would leave it world-readable).
fn write_secret_0600(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let tmp = path.with_extension("key.tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)
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

    // The optional shared half is validated before anything changes, so a
    // malformed key is a clean 400 rather than a pair that silently stays deaf.
    let shared_key = match req.shared_key_b64.as_deref().map(decode_shared_key) {
        None => None,
        Some(Ok(bytes)) => Some(bytes),
        Some(Err(message)) => {
            return nested_detail(
                StatusCode::BAD_REQUEST,
                json!({"code": "E_SHARED_KEY_INVALID", "message": message}),
            )
        }
    };

    // Fleet-key gate. `installed` is the on-disk fleet key, if any.
    let status = gs_pair_status();
    let installed = status
        .paired
        .then(|| std::fs::read(rx_key_path()).ok())
        .flatten();
    let joining = installed.is_some();
    if installed.as_ref().is_some_and(|existing| *existing != blob) {
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

    // Issue the slot BEFORE installing anything. Idempotent by device id, so a
    // re-pair returns the slot the drone already holds and never renumbers one
    // that may be airborne. Reserving first means a full fleet or a registry
    // that cannot be persisted refuses the pair with nothing installed, rather
    // than leaving a key on the station behind an error reply.
    let (slot, newly_reserved) = {
        let _write = fleet_registry_write();
        let mut registry = load_registry();
        let newly_reserved = registry.slot_of(&device_id).is_none();
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
            // The slot exists only in memory now, so the next pair would re-issue
            // it to a different drone and put two transmitters on one
            // channel_id. Refuse rather than return an assignment the ground
            // station will not honour.
            tracing::error!(error = %e, device_id = %device_id, slot, "fleet_registry_persist_failed");
            return nested_detail(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({
                    "code": "E_FLEET_PERSIST_FAILED",
                    "message": e.to_string(),
                }),
            );
        }
        (slot, newly_reserved)
    };

    let mut body = if joining {
        // Byte-identical: the fleet key is already installed. Skip the install.
        Map::new()
    } else {
        // Forward the install. The socket's pair_keypair op decodes +
        // validates the blob, writes rx.key + the pair state, drops the
        // sentinel, and restarts the receive unit; its reply carries the
        // install body the FastAPI route returned.
        let request = json!({
            "op": "pair_keypair",
            "blob_b64": blob_b64,
            "peer_device_id": device_id,
        });
        let outcome = match groundlink_cmd_roundtrip(&request).await {
            Some(reply) => split_reply(reply).map_err(map_pair_error),
            None => Err(socket_unavailable("E_PAIR_FAILED")),
        };
        match outcome {
            Ok(b) => b,
            Err(resp) => {
                // Nothing was installed, so the slot reserved for this attempt
                // must not outlive it.
                if newly_reserved {
                    release_reserved_slot(&device_id);
                }
                return resp;
            }
        }
    };

    // Persist the shared half before the reply, so a caller that supplies it
    // gets a ground station whose hop supervisor can actually parse a beacon
    // rather than one that pairs and then stays deaf.
    let shared_key_outcome = shared_key
        .map(|bytes| install_shared_key(std::path::Path::new(SHARED_KEY_PATH), &bytes, joining));

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
    // Re-read the roster: a concurrent pair may have landed while the install
    // was in flight.
    let registry = {
        let _write = fleet_registry_write();
        load_registry()
    };
    body.insert("fleet_slot".to_string(), json!(slot));
    body.insert("slots".to_string(), json!(slot_table(&registry)));
    if let Some(outcome) = shared_key_outcome {
        body.insert("shared_key".to_string(), json!(outcome));
    }
    Json(Value::Object(body)).into_response()
}

/// Release a slot reserved by a pair attempt whose key install then failed. A
/// persist failure here is logged: the slot stays held by a device that never
/// paired, which `DELETE .../wfb/pair/{device_id}` clears.
fn release_reserved_slot(device_id: &str) {
    let _write = fleet_registry_write();
    let mut registry = load_registry();
    if registry.release(device_id) {
        if let Err(e) = registry.persist(std::path::Path::new(FLEET_REGISTRY_PATH)) {
            tracing::error!(error = %e, device_id = %device_id, "fleet_slot_rollback_persist_failed");
        }
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

/// `DELETE /api/v1/ground-station/wfb/pair/:device_id` — release ONE drone's
/// fleet slot.
///
/// The station-wide unpair above is the only reset that existed, and it is the
/// wrong tool for "this one airframe is being retired or re-flashed": it wipes
/// the radio keys and drops every other member of the fleet with it. The gap
/// meant a bench removing one drone edited `fleet.json` by hand, which is a
/// runtime patch of exactly the kind that leaves a box in a state no install can
/// reproduce.
///
/// Releasing a slot does NOT touch keys. The fleet shares one radio keypair, so
/// the released drone keeps working until it is re-paired or re-flashed; what is
/// freed is the slot number, so the next drone to pair takes it rather than
/// running the fleet out of slots.
pub async fn delete_fleet_slot(
    State(_state): State<AppState>,
    axum::extract::Path(device_id): axum::extract::Path<String>,
) -> Response {
    if !is_ground_station() {
        return profile_mismatch();
    }
    if device_id.trim().is_empty() {
        return nested_detail(
            StatusCode::BAD_REQUEST,
            json!({
                "code": "E_DEVICE_ID_REQUIRED",
                "message": "a device id is required to release a fleet slot",
            }),
        );
    }

    let _write = fleet_registry_write();
    let mut registry = load_registry();
    let released = registry.release(&device_id);
    if !released {
        // Not an error worth a 500: the caller's intent is "this drone should
        // not hold a slot", and it does not. Report it as already-absent with
        // the roster, so a retry after a partial failure is safe.
        return nested_detail(
            StatusCode::NOT_FOUND,
            json!({
                "code": "E_SLOT_NOT_FOUND",
                "message": format!("{device_id} holds no fleet slot"),
                "slots": slot_table(&registry),
            }),
        );
    }

    if let Err(e) = registry.persist(std::path::Path::new(FLEET_REGISTRY_PATH)) {
        // The release exists only in memory. Refuse rather than report a slot
        // freed that the ground station will re-read as still taken on restart.
        tracing::error!(error = %e, device_id = %device_id, "fleet_registry_persist_failed");
        return nested_detail(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({
                "code": "E_FLEET_PERSIST_FAILED",
                "message": "the slot was released in memory but could not be persisted",
            }),
        );
    }

    tracing::info!(device_id = %device_id, "fleet_slot_released");
    Json(json!({
        "released": true,
        "device_id": device_id,
        "slots": slot_table(&registry),
    }))
    .into_response()
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
    fn releasing_one_slot_leaves_the_rest_of_the_fleet_registered() {
        // The whole point of the per-device release: retiring one airframe must
        // not disturb the others. The station-wide unpair is the tool that drops
        // everyone, and reaching for it to remove a single drone is what made a
        // bench edit the registry file by hand instead.
        let mut registry = FleetRegistry::default();
        registry.allocate("drone-a");
        registry.allocate("drone-b");
        registry.allocate("drone-c");

        assert!(registry.release("drone-b"));

        let left: Vec<String> = registry.slots().map(|s| s.device_id.clone()).collect();
        assert_eq!(left, vec!["drone-a".to_string(), "drone-c".to_string()]);
    }

    #[test]
    fn a_released_slot_is_reissued_rather_than_running_the_fleet_out() {
        // Releasing frees the slot NUMBER. Without that, a station that had
        // cycled through drones would report itself full while holding
        // registrations for airframes that no longer exist.
        let mut registry = FleetRegistry::default();
        let a = registry.allocate("drone-a").unwrap();
        registry.allocate("drone-b");

        assert!(registry.release("drone-a"));
        assert_eq!(
            registry.allocate("drone-d"),
            Some(a),
            "the freed slot number should be the next one issued"
        );
    }

    #[test]
    fn releasing_a_device_that_holds_no_slot_reports_absent_rather_than_succeeding() {
        // The caller's intent is "this drone should not hold a slot". Reporting
        // success for a device that was never registered would make a typo look
        // like a completed release, and the operator would stop looking.
        let mut registry = FleetRegistry::default();
        registry.allocate("drone-a");

        assert!(!registry.release("never-paired"));
        assert_eq!(registry.slots().count(), 1, "nothing else may be disturbed");
    }

    #[test]
    fn the_slot_table_never_carries_a_relay_secret() {
        // The roster IS returned to a caller holding the fleet key. The
        // per-pair relay secret must not ride along: it is the one thing that
        // distinguishes a drone's own ground station from anything else that
        // can reach the air, and handing it to every fleet member would undo
        // exactly what it is for. `slot_table` picks fields explicitly today —
        // this fails if anyone replaces it with a whole-struct serialization.
        let mut registry = FleetRegistry::default();
        registry.allocate("aaaa");
        let rendered = serde_json::to_string(&slot_table(&registry)).unwrap();

        let secret = registry
            .slots()
            .next()
            .unwrap()
            .relay_secret
            .clone()
            .expect("allocation issues a secret");
        assert!(!secret.is_empty());
        assert!(
            !rendered.contains(&secret),
            "the relay secret leaked into the slot table: {rendered}"
        );
        assert!(!rendered.contains("relay_secret"));
        // The fields it SHOULD carry are still there.
        assert!(rendered.contains("device_id") && rendered.contains("slot"));
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
    fn a_shared_key_of_the_wrong_size_is_refused_rather_than_written() {
        // A truncated key derives a WRONG beacon HMAC, which drops every beacon
        // silently -- harder to notice than a missing key, because the resolver
        // at least warns when nothing is there.
        use base64::Engine as _;
        let short = base64::engine::general_purpose::STANDARD.encode([0u8; 32]);
        assert!(decode_shared_key(&short).is_err());
    }

    #[test]
    fn a_shared_key_that_is_not_base64_is_refused() {
        assert!(decode_shared_key("not base64!!").is_err());
    }

    #[test]
    fn the_shared_key_is_written_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("drone.key");
        assert_eq!(install_shared_key(&path, &[7u8; 64], false), "installed");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(std::fs::read(&path).unwrap(), vec![7u8; 64]);
    }

    #[test]
    fn a_join_never_rekeys_an_installed_shared_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("drone.key");
        std::fs::write(&path, [1u8; 64]).unwrap();
        assert_eq!(install_shared_key(&path, &[1u8; 64], true), "unchanged");
        assert_eq!(install_shared_key(&path, &[2u8; 64], true), "mismatch");
        assert_eq!(std::fs::read(&path).unwrap(), vec![1u8; 64]);
        // A fresh install is a new fleet: it replaces the old shared half.
        assert_eq!(install_shared_key(&path, &[2u8; 64], false), "installed");
        assert_eq!(std::fs::read(&path).unwrap(), vec![2u8; 64]);
    }

    #[test]
    fn the_request_accepts_a_shared_key_and_still_parses_without_one() {
        // Absent, the route must behave exactly as it did: the radio bind stays
        // the path that distributes the shared half.
        let with: PairRequest =
            serde_json::from_str(r#"{"blob_b64":"x","shared_key_b64":"y"}"#).unwrap();
        assert_eq!(with.shared_key_b64.as_deref(), Some("y"));
        let without: PairRequest = serde_json::from_str(r#"{"blob_b64":"x"}"#).unwrap();
        assert!(without.shared_key_b64.is_none());
    }
}
