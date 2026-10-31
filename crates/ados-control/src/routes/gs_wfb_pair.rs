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
//! The station-wide factory reset, which also unpairs, is
//! [`crate::routes::gs_factory_reset`].
//!
//! ## The fleet gate
//!
//! `POST .../wfb/pair` runs the guards in order: profile gate (404
//! `E_PROFILE_MISMATCH`); the deprecated-`pair_key` 400; the missing-`blob_b64`
//! 400; the missing-`drone_device_id` 400 (`E_DEVICE_ID_REQUIRED`) or a
//! malformed one (`E_DEVICE_ID_INVALID`, see
//! [`ados_groundlink::is_valid_device_id`]); then the FLEET gate.
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

use ados_groundlink::aux_peers::SlotSystemId;
use ados_groundlink::{FleetRegistry, FLEET_MAX_SLOTS, FLEET_REGISTRY_PATH};

use crate::routes::gs_cmd::groundlink_cmd_roundtrip;
use crate::state::AppState;
use crate::wfb_pair_state::read_public_fingerprint;

mod pair;

pub(crate) use pair::fleet_registry_write;
pub use pair::post_wfb_pair;
pub use pair::PairRequest;
#[cfg(test)]
use pair::*;

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

/// True when the resolved profile is a ground station (config `agent.profile`
/// plus the on-disk sentinels).
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

/// Read the GS pair status. A status read fault is treated as not-paired; an
/// unreadable registry is an empty slot table, never a failure.
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
///
/// Each row also carries the flight-controller identity the receive chain
/// currently hears on that slot: `fc_system_id` (null until a HEARTBEAT has
/// been heard recently) and `system_id_conflict` (another linked slot is
/// heartbeating with the same system id, so commands to it are held).
///
/// `video_hero` marks the one slot whose video the ground station is serving
/// on its `main` stream. Only that slot's video is decoded, so every other
/// drone in the fleet has no live feed here until it is made the hero.
pub(crate) fn slot_table(registry: &FleetRegistry) -> Vec<Value> {
    slot_table_with(
        registry,
        &load_slot_system_ids(),
        served_video_slot(registry),
    )
}

/// [`slot_table`] against an explicit identity map and served slot, so the
/// shape is testable without a live receive process.
fn slot_table_with(
    registry: &FleetRegistry,
    ids: &std::collections::BTreeMap<u8, SlotSystemId>,
    video_slot: Option<u8>,
) -> Vec<Value> {
    registry
        .slots()
        .map(|s| {
            let id = ids.get(&s.slot);
            json!({
                "slot": s.slot,
                "device_id": s.device_id,
                "paired_at_ms": s.paired_at_ms,
                "fc_system_id": id.map(|i| i.fc_system_id),
                "system_id_conflict": id.is_some_and(|i| i.conflict),
                "video_hero": video_slot == Some(s.slot),
                // Whether the drone holds this ground station's relay secret:
                // `pending`, `held`, or `conflict` (it holds another station's
                // and relayed calls to it are refused until it is unpaired).
                "relay_credential": crate::routes::gs_fleet_slot::relay_secret_state(&s.device_id),
            })
        })
        .collect()
}

/// The slot the ground station's video fan-out serves: the published hero
/// while it is still registered, otherwise the lowest registered slot, which
/// is the receive chain's primary. Resolved by the same function the fan-out
/// runs, so the roster and the stream cannot disagree.
fn served_video_slot(registry: &FleetRegistry) -> Option<u8> {
    let primary = registry.slots().map(|s| s.slot).min()?;
    Some(ados_groundlink::fanout::resolve_fanout_slot(
        primary,
        std::path::Path::new(&ados_groundlink::hero_path()),
        std::path::Path::new(FLEET_REGISTRY_PATH),
    ))
}

/// How old the relayed-status document may be before its identities are not
/// served. The writer rewrites it every two seconds.
const SLOT_IDS_DOC_STALE_AFTER_S: f64 = 20.0;

/// The per-slot flight-controller identities the receive chain publishes in
/// the relayed-status sidecar, re-aged now. Absent or stale reads as empty.
fn load_slot_system_ids() -> std::collections::BTreeMap<u8, SlotSystemId> {
    let path = std::path::PathBuf::from(
        std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string()),
    )
    .join(ados_groundlink::aux_peers::AUX_PEERS_SIDECAR);
    let Some(doc) = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    else {
        return Default::default();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    ados_groundlink::aux_peers::slot_system_ids_from_sidecar(&doc, now, SLOT_IDS_DOC_STALE_AFTER_S)
}

// ---------------------------------------------------------------------------
// DELETE /api/v1/ground-station/wfb/pair — wipe the GS pair key.
// ---------------------------------------------------------------------------

/// `DELETE .../wfb/pair` → `{paired:false, role:"gs"}`.
///
/// Gates on the profile, then forwards the `unpair` op (which wipes both key
/// files, clears the persisted pair state, restarts the receive unit) and returns
/// its reply. A socket-reported failure is a 500 `E_UNPAIR_FAILED`; an
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
/// unreachable. The front cannot install/wipe the key itself, so an absent socket
/// degrades to a 503 with the route's error code rather than a 500.
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
mod tests;
