//! Ground-station phone-receiver invites: an operator-approved, sealed hand-off
//! of the fleet's WFB receive material to a phone that listens on its own radio.
//!
//! A phone generates an X25519 keypair and posts its public half
//! (`POST .../wfb/invite`). The invite sits pending, showing the phone's
//! fingerprint, until an operator approves or rejects it on a ground-station
//! surface. Approval seals the receive bundle to the phone's key under a fresh
//! ephemeral key (the mesh-invite construction from
//! [`ados_groundlink::pairing::crypto`], context
//! `"ados/wfb/phone-invite/v1" || invite_id`); the phone fetches it exactly once
//! (`GET .../wfb/invite/{id}`), after which it is gone.
//!
//! The bundle carries the fleet's `rx.key` and the DERIVED hop HMAC key, never
//! the raw shared key file. The phone is receive-only by design (it never
//! transmits on the ground slot), but the material is not receive-only: wfb-ng's
//! key pair also authenticates frames, so an approved phone is trusted by the
//! fleet as fully as the ground station itself. Approve only a phone you own.
//!
//! Every route is ground-station only (404 `E_PROFILE_MISMATCH` elsewhere) and
//! listed in [`crate::auth::RELAY_FORBIDDEN_PATHS`], so no relayed request can
//! mint, approve or collect an invite. State is in memory: a restart drops every
//! invite, which costs the phone one retry and leaves nothing on disk.
//!
//! State machine, per invite: `pending` → `approved` (sealed) → fetched (gone);
//! `pending` → `rejected`; `pending`/`approved` past their window → expired.
//! A terminal invite answers its fetch with 403 (rejected) or 410 (fetched or
//! expired) for a while before it is forgotten.

use std::path::PathBuf;

use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use ados_groundlink::pairing::{seal_to_peer, SealedToPeer};

mod store;

pub use store::FetchOutcome;
pub use store::InviteError;
pub use store::InviteStore;
use store::*;

/// How long a pending invite waits for the operator, and how long an approved
/// bundle waits for the phone to collect it.
pub const INVITE_TTL_MS: i64 = 120_000;

/// At most this many invites may be pending at once.
pub const MAX_PENDING: usize = 4;

/// How long a terminal invite keeps answering its fetch (403 / 410) before it is
/// forgotten and reads as not found.
const TOMBSTONE_MS: i64 = 600_000;

/// The fixed prefix of the sealing context; the invite id follows it directly.
pub const PHONE_INVITE_CONTEXT: &[u8] = b"ados/wfb/phone-invite/v1";

/// The longest operator-visible label kept, in characters.
const LABEL_MAX_CHARS: usize = 64;

/// The radio's channel width the phone receives on.
const BANDWIDTH_MHZ: u8 = 20;

// ---------------------------------------------------------------------------
// Bundle material.
// ---------------------------------------------------------------------------

/// What the ground station hands an approved phone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteMaterial {
    /// The fleet's 64-byte wfb-ng receive key.
    pub rx_key: Vec<u8>,
    /// The derived hop/presence HMAC key (never the raw shared key file).
    pub hop_key: [u8; 32],
    pub fleet_id: u16,
    /// `(slot, device_id)` for every registered drone.
    pub slots: Vec<(u8, String)>,
    pub channel: u8,
    pub rendezvous_channel: u8,
    pub mcs: u8,
    pub video_fec: (u8, u8),
    pub aux_fec: (u8, u8),
}

/// The sealing context for one invite.
pub fn invite_context(invite_id: &str) -> Vec<u8> {
    let mut ctx = PHONE_INVITE_CONTEXT.to_vec();
    ctx.extend_from_slice(invite_id.as_bytes());
    ctx
}

/// The plaintext bundle the phone opens.
pub fn bundle_plaintext(m: &InviteMaterial) -> Vec<u8> {
    let b64 = base64::engine::general_purpose::STANDARD;
    let slots: Vec<Value> = m
        .slots
        .iter()
        .map(|(slot, device_id)| json!({"slot": slot, "device_id": device_id}))
        .collect();
    let body = json!({
        "v": 1,
        "rx_key_b64": b64.encode(&m.rx_key),
        "hop_key_b64": b64.encode(m.hop_key),
        "fleet_id": m.fleet_id,
        "slots": slots,
        "channel": m.channel,
        "rendezvous_channel": m.rendezvous_channel,
        "bandwidth_mhz": BANDWIDTH_MHZ,
        "mcs": m.mcs,
        "video_fec": {"k": m.video_fec.0, "n": m.video_fec.1},
        "aux_fec": {"k": m.aux_fec.0, "n": m.aux_fec.1},
        "ports": {"video": 0, "control": 1, "aux_down": 2},
    });
    serde_json::to_vec(&body).expect("a json! value always serializes")
}

/// The phone fingerprint an operator compares against the phone's screen: the
/// first 8 bytes of SHA-256 over the raw public key, uppercase hex in four
/// dash-separated groups.
pub fn phone_fingerprint(phone_pub: &[u8; 32]) -> String {
    let digest = Sha256::digest(phone_pub);
    let hex = hex::encode_upper(&digest[..8]);
    format!(
        "{}-{}-{}-{}",
        &hex[0..4],
        &hex[4..8],
        &hex[8..12],
        &hex[12..16]
    )
}

/// Why the receive material could not be read.
#[derive(Debug, PartialEq, Eq)]
pub enum MaterialError {
    /// No whole `rx.key`: the ground station has no fleet key to share.
    NotPaired,
    /// The shared key file exists but is unreadable, so no hop key can be
    /// derived without downgrading to the public cold-start constant.
    HopKeyUnavailable,
}

/// The ground station's agent config path.
fn config_yaml_path() -> PathBuf {
    PathBuf::from(
        std::env::var("ADOS_CONFIG").unwrap_or_else(|_| crate::config::CONFIG_YAML.to_string()),
    )
}

/// The GS rx-side key file, honouring `ADOS_WFB_KEY_DIR` like the pair routes.
fn rx_key_path() -> PathBuf {
    std::env::var("ADOS_WFB_KEY_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/etc/ados/wfb"))
        .join("rx.key")
}

/// Read the live receive material: `rx.key`, the derived hop key, the fleet
/// registry and the radio config.
fn load_material() -> Result<InviteMaterial, MaterialError> {
    let rx_key = std::fs::read(rx_key_path()).map_err(|_| MaterialError::NotPaired)?;
    if rx_key.len() != 64 {
        return Err(MaterialError::NotPaired);
    }
    let shared = ados_radio::paths::load_shared_key();
    let hop_key = ados_radio::hop::pair_key_for(&shared).ok_or(MaterialError::HopKeyUnavailable)?;
    let cfg = ados_radio::config::WfbConfig::load_from(&config_yaml_path());
    let registry = crate::routes::gs_wfb_pair::load_registry();
    let slots = registry
        .slots()
        .map(|s| (s.slot, s.device_id.clone()))
        .collect();
    Ok(InviteMaterial {
        rx_key,
        hop_key,
        fleet_id: cfg.fleet_id,
        slots,
        channel: cfg.channel,
        rendezvous_channel: cfg.rendezvous_channel(),
        mcs: cfg.mcs_index,
        video_fec: (cfg.fec_k, cfg.fec_n),
        aux_fec: (cfg.aux_fec_k, cfg.aux_fec_n),
    })
}

// ---------------------------------------------------------------------------
// HTTP mapping.
// ---------------------------------------------------------------------------

fn nested_detail(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({ "detail": { "error": { "code": code, "message": message } } })),
    )
        .into_response()
}

fn profile_mismatch() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "detail": { "error": { "code": "E_PROFILE_MISMATCH" } } })),
    )
        .into_response()
}

fn error_response(err: InviteError) -> Response {
    match err {
        InviteError::PhonePubInvalid => nested_detail(
            StatusCode::BAD_REQUEST,
            "E_PHONE_PUB_INVALID",
            "phone_pub_b64 must be a base64 32-byte X25519 public key",
        ),
        InviteError::TooManyPending => nested_detail(
            StatusCode::TOO_MANY_REQUESTS,
            "E_INVITE_LIMIT",
            "too many invites are already waiting for approval",
        ),
        InviteError::NotFound => nested_detail(
            StatusCode::NOT_FOUND,
            "E_INVITE_NOT_FOUND",
            "no such invite",
        ),
        InviteError::NotPending => nested_detail(
            StatusCode::CONFLICT,
            "E_INVITE_NOT_PENDING",
            "the invite has already been decided",
        ),
        InviteError::Expired => nested_detail(
            StatusCode::GONE,
            "E_INVITE_EXPIRED",
            "the invite has expired",
        ),
        InviteError::Material(MaterialError::NotPaired) => nested_detail(
            StatusCode::CONFLICT,
            "E_GS_NOT_PAIRED",
            "this ground station holds no fleet receive key",
        ),
        InviteError::Material(MaterialError::HopKeyUnavailable) => nested_detail(
            StatusCode::SERVICE_UNAVAILABLE,
            "E_HOP_KEY_UNAVAILABLE",
            "the shared key file is unreadable",
        ),
        InviteError::SealFailed => nested_detail(
            StatusCode::BAD_REQUEST,
            "E_PHONE_PUB_INVALID",
            "the bundle cannot be sealed to this phone key",
        ),
    }
}

fn fetch_response(outcome: FetchOutcome) -> Response {
    let b64 = base64::engine::general_purpose::STANDARD;
    match outcome {
        FetchOutcome::Pending => {
            (StatusCode::ACCEPTED, Json(json!({"state": "pending"}))).into_response()
        }
        FetchOutcome::Sealed(sealed) => (
            StatusCode::OK,
            Json(json!({
                "state": "approved",
                "gs_eph_pub_b64": b64.encode(sealed.eph_pub),
                "nonce_b64": b64.encode(sealed.nonce),
                "ciphertext_b64": b64.encode(&sealed.ciphertext),
            })),
        )
            .into_response(),
        FetchOutcome::Rejected => {
            (StatusCode::FORBIDDEN, Json(json!({"state": "rejected"}))).into_response()
        }
        FetchOutcome::Expired => {
            (StatusCode::GONE, Json(json!({"state": "expired"}))).into_response()
        }
    }
}

fn is_ground_station() -> bool {
    let cfg = crate::config::PairingConfig::load();
    let (profile, _role) = crate::profile::current_profile_and_role(&cfg.agent.profile);
    profile == "ground-station"
}

fn new_invite_id() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("OS RNG for invite id");
    hex::encode(bytes)
}

fn now_ms() -> i64 {
    ados_groundlink::pairing::crypto::now_ms()
}

/// `POST .../wfb/invite` body.
#[derive(Debug, Deserialize)]
pub struct CreateInviteRequest {
    pub phone_pub_b64: String,
    #[serde(default)]
    pub label: Option<String>,
}

// The handlers below are split into a gate-taking core (`*_with`) so the profile
// gate and the store are testable without the process config or the global.

fn create_with(
    gs: bool,
    store: &mut InviteStore,
    req: &CreateInviteRequest,
    invite_id: String,
    rx_key_present: bool,
    now_ms: i64,
) -> Response {
    if !gs {
        return profile_mismatch();
    }
    if !rx_key_present {
        return error_response(InviteError::Material(MaterialError::NotPaired));
    }
    match store.create(&req.phone_pub_b64, req.label.as_deref(), invite_id, now_ms) {
        Ok(body) => (StatusCode::CREATED, Json(body)).into_response(),
        Err(e) => error_response(e),
    }
}

fn list_with(gs: bool, store: &mut InviteStore, now_ms: i64) -> Response {
    if !gs {
        return profile_mismatch();
    }
    Json(store.list_pending(now_ms)).into_response()
}

fn approve_with(
    gs: bool,
    store: &mut InviteStore,
    id: &str,
    material: impl FnOnce() -> Result<InviteMaterial, MaterialError>,
    now_ms: i64,
) -> Response {
    if !gs {
        return profile_mismatch();
    }
    match store.approve(id, material(), now_ms) {
        Ok(()) => Json(json!({"invite_id": id, "state": "approved"})).into_response(),
        Err(e) => error_response(e),
    }
}

fn reject_with(gs: bool, store: &mut InviteStore, id: &str, now_ms: i64) -> Response {
    if !gs {
        return profile_mismatch();
    }
    match store.reject(id, now_ms) {
        Ok(()) => Json(json!({"invite_id": id, "state": "rejected"})).into_response(),
        Err(e) => error_response(e),
    }
}

fn fetch_with(gs: bool, store: &mut InviteStore, id: &str, now_ms: i64) -> Response {
    if !gs {
        return profile_mismatch();
    }
    match store.fetch(id, now_ms) {
        Ok(outcome) => fetch_response(outcome),
        Err(e) => error_response(e),
    }
}

/// `POST /api/v1/ground-station/wfb/invite` →
/// `201 {invite_id, phone_fingerprint, expires_at_ms}`.
pub async fn post_invite(Json(req): Json<CreateInviteRequest>) -> Response {
    let gs = is_ground_station();
    let rx_key_present = std::fs::metadata(rx_key_path())
        .map(|m| m.len() == 64)
        .unwrap_or(false);
    create_with(
        gs,
        &mut STORE.lock(),
        &req,
        new_invite_id(),
        rx_key_present,
        now_ms(),
    )
}

/// `GET /api/v1/ground-station/wfb/invite` → `{pending:[…]}`.
pub async fn list_invites() -> Response {
    list_with(is_ground_station(), &mut STORE.lock(), now_ms())
}

/// `POST /api/v1/ground-station/wfb/invite/{id}/approve`.
pub async fn approve_invite(Path(id): Path<String>) -> Response {
    let gs = is_ground_station();
    // Read the material before taking the lock: it touches the disk.
    let material = if gs {
        load_material()
    } else {
        Err(MaterialError::NotPaired)
    };
    approve_with(gs, &mut STORE.lock(), &id, || material, now_ms())
}

/// `POST /api/v1/ground-station/wfb/invite/{id}/reject`.
pub async fn reject_invite(Path(id): Path<String>) -> Response {
    reject_with(is_ground_station(), &mut STORE.lock(), &id, now_ms())
}

/// `GET /api/v1/ground-station/wfb/invite/{id}` → 202 pending | 200 sealed
/// bundle (once) | 403 rejected | 410 expired.
pub async fn get_invite(Path(id): Path<String>) -> Response {
    fetch_with(is_ground_station(), &mut STORE.lock(), &id, now_ms())
}

#[cfg(test)]
mod tests;
