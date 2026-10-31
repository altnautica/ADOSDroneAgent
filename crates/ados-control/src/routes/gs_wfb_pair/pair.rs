//! `POST /api/v1/ground-station/wfb/pair`: install the GS rx-side key.

use super::*;

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
pub(super) const SHARED_KEY_PATH: &str = "/etc/drone.key";

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
pub(super) fn decode_shared_key(b64: &str) -> Result<Vec<u8>, String> {
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
pub(super) fn install_shared_key(
    path: &std::path::Path,
    bytes: &[u8],
    joining: bool,
) -> &'static str {
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
pub(super) fn write_secret_0600(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
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

/// The device id a pair is issued to, or the 400 that refuses it.
///
/// A slot is issued TO a device and `FleetRegistry::allocate` is idempotent by
/// device id, so without one every re-pair would burn a fresh slot until the
/// fleet reported full. A blank or one-character id is refused too: stored as
/// given, it could never be matched to the id the aircraft beacons for itself.
pub(super) fn pair_device_id(raw: Option<String>) -> Result<String, Box<Response>> {
    let Some(id) = raw.filter(|s| !s.is_empty()) else {
        return Err(Box::new(nested_detail(
            StatusCode::BAD_REQUEST,
            json!({
                "code": "E_DEVICE_ID_REQUIRED",
                "message": "drone_device_id is required: a fleet slot is issued to a device and re-pairing is matched by it",
            }),
        )));
    };
    if !ados_groundlink::is_valid_device_id(&id) {
        return Err(Box::new(nested_detail(
            StatusCode::BAD_REQUEST,
            json!({
                "code": "E_DEVICE_ID_INVALID",
                "message": "drone_device_id must be 8 to 32 characters of letters, digits, '-', '_' or '.'",
            }),
        )));
    }
    Ok(id)
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

    // The typed `pair_key` is no longer supported; surface a clear 400 when an
    // old client sends it without a blob.
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

    let device_id = match pair_device_id(req.drone_device_id) {
        Ok(id) => id,
        Err(resp) => return *resp,
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
        // install body.
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
pub(super) fn release_reserved_slot(device_id: &str) {
    let _write = fleet_registry_write();
    let mut registry = load_registry();
    if registry.release(device_id) {
        if let Err(e) = registry.persist(std::path::Path::new(FLEET_REGISTRY_PATH)) {
            tracing::error!(error = %e, device_id = %device_id, "fleet_slot_rollback_persist_failed");
        }
    }
}

/// Map a `pair_keypair` failure reply to the status + body. The op returns
/// `E_BLOB_BASE64` for an undecodable blob, `E_INVALID_KEY_BLOB` for a wrong length
/// (both 400), and `E_PAIR_FAILED` for an IO fault (500). An unexpected code is
/// treated as a 500 `E_PAIR_FAILED` (the catch-all).
pub(super) fn map_pair_error(err: SocketError) -> Response {
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
