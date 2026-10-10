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
//! the raw shared key file. The phone receives only: it holds no transmit key.
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
    format!("{}-{}-{}-{}", &hex[0..4], &hex[4..8], &hex[8..12], &hex[12..16])
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
    let hop_key =
        ados_radio::hop::pair_key_for(&shared).ok_or(MaterialError::HopKeyUnavailable)?;
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
// Invite store.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum InviteState {
    Pending,
    Approved(SealedToPeer),
    Rejected,
    /// Fetched or expired: the bundle no longer exists.
    Gone,
}

#[derive(Debug, Clone)]
struct Invite {
    id: String,
    label: String,
    phone_pub: [u8; 32],
    fingerprint: String,
    /// End of the current window: the operator's (pending) or the phone's
    /// collection window (approved).
    expires_at_ms: i64,
    state: InviteState,
    /// When the invite reached a terminal state.
    terminal_at_ms: Option<i64>,
}

/// Every refusal the invite routes return.
#[derive(Debug, PartialEq, Eq)]
pub enum InviteError {
    PhonePubInvalid,
    TooManyPending,
    NotFound,
    NotPending,
    Expired,
    Material(MaterialError),
    SealFailed,
}

/// What a fetch found.
#[derive(Debug, PartialEq, Eq)]
pub enum FetchOutcome {
    Pending,
    Sealed(SealedToPeer),
    Rejected,
    Expired,
}

/// The in-memory invite table.
#[derive(Debug, Default)]
pub struct InviteStore {
    invites: Vec<Invite>,
}

impl InviteStore {
    pub const fn new() -> Self {
        Self {
            invites: Vec::new(),
        }
    }

    /// Expire overdue invites and forget old terminal ones.
    fn sweep(&mut self, now_ms: i64) {
        for inv in &mut self.invites {
            let live = matches!(inv.state, InviteState::Pending | InviteState::Approved(_));
            if live && now_ms >= inv.expires_at_ms {
                inv.state = InviteState::Gone;
                inv.terminal_at_ms = Some(now_ms);
                tracing::info!(
                    invite_id = %inv.id,
                    phone_fingerprint = %inv.fingerprint,
                    "wfb_phone_invite_expired"
                );
            }
        }
        self.invites
            .retain(|inv| inv.terminal_at_ms.is_none_or(|t| now_ms - t < TOMBSTONE_MS));
    }

    fn find(&mut self, id: &str) -> Result<&mut Invite, InviteError> {
        self.invites
            .iter_mut()
            .find(|inv| inv.id == id)
            .ok_or(InviteError::NotFound)
    }

    /// Register a phone's public key as a pending invite.
    pub fn create(
        &mut self,
        phone_pub_b64: &str,
        label: Option<&str>,
        invite_id: String,
        now_ms: i64,
    ) -> Result<Value, InviteError> {
        self.sweep(now_ms);
        let phone_pub: [u8; 32] = base64::engine::general_purpose::STANDARD
            .decode(phone_pub_b64.trim())
            .ok()
            .and_then(|b| b.try_into().ok())
            .ok_or(InviteError::PhonePubInvalid)?;
        let pending = self
            .invites
            .iter()
            .filter(|inv| matches!(inv.state, InviteState::Pending))
            .count();
        if pending >= MAX_PENDING {
            return Err(InviteError::TooManyPending);
        }
        let label = label
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .unwrap_or("Phone")
            .chars()
            .filter(|c| !c.is_control())
            .take(LABEL_MAX_CHARS)
            .collect::<String>();
        let fingerprint = phone_fingerprint(&phone_pub);
        let expires_at_ms = now_ms + INVITE_TTL_MS;
        tracing::info!(
            invite_id = %invite_id,
            phone_fingerprint = %fingerprint,
            "wfb_phone_invite_created"
        );
        let body = json!({
            "invite_id": invite_id,
            "phone_fingerprint": fingerprint,
            "expires_at_ms": expires_at_ms,
        });
        self.invites.push(Invite {
            id: invite_id,
            label,
            phone_pub,
            fingerprint,
            expires_at_ms,
            state: InviteState::Pending,
            terminal_at_ms: None,
        });
        Ok(body)
    }

    /// The pending invites, oldest first.
    pub fn list_pending(&mut self, now_ms: i64) -> Value {
        self.sweep(now_ms);
        let pending: Vec<Value> = self
            .invites
            .iter()
            .filter(|inv| matches!(inv.state, InviteState::Pending))
            .map(|inv| {
                json!({
                    "invite_id": inv.id,
                    "label": inv.label,
                    "phone_fingerprint": inv.fingerprint,
                    "expires_at_ms": inv.expires_at_ms,
                })
            })
            .collect();
        json!({ "pending": pending })
    }

    fn pending_invite(&mut self, id: &str, now_ms: i64) -> Result<&mut Invite, InviteError> {
        self.sweep(now_ms);
        let inv = self.find(id)?;
        match inv.state {
            InviteState::Pending => Ok(inv),
            InviteState::Gone => Err(InviteError::Expired),
            InviteState::Approved(_) | InviteState::Rejected => Err(InviteError::NotPending),
        }
    }

    /// Approve a pending invite: seal the bundle to the phone's key now.
    pub fn approve(
        &mut self,
        id: &str,
        material: Result<InviteMaterial, MaterialError>,
        now_ms: i64,
    ) -> Result<(), InviteError> {
        let inv = self.pending_invite(id, now_ms)?;
        let material = material.map_err(InviteError::Material)?;
        let sealed = seal_to_peer(
            &inv.phone_pub,
            &invite_context(&inv.id),
            &bundle_plaintext(&material),
        )
        .map_err(|_| InviteError::SealFailed)?;
        inv.state = InviteState::Approved(sealed);
        inv.expires_at_ms = now_ms + INVITE_TTL_MS;
        tracing::info!(
            invite_id = %inv.id,
            phone_fingerprint = %inv.fingerprint,
            "wfb_phone_invite_approved"
        );
        Ok(())
    }

    /// Reject a pending invite.
    pub fn reject(&mut self, id: &str, now_ms: i64) -> Result<(), InviteError> {
        let inv = self.pending_invite(id, now_ms)?;
        inv.state = InviteState::Rejected;
        inv.terminal_at_ms = Some(now_ms);
        tracing::info!(
            invite_id = %inv.id,
            phone_fingerprint = %inv.fingerprint,
            "wfb_phone_invite_rejected"
        );
        Ok(())
    }

    /// The phone's poll. A sealed bundle is handed out once, then deleted.
    pub fn fetch(&mut self, id: &str, now_ms: i64) -> Result<FetchOutcome, InviteError> {
        self.sweep(now_ms);
        let inv = self.find(id)?;
        let outcome = match std::mem::replace(&mut inv.state, InviteState::Gone) {
            InviteState::Pending => {
                inv.state = InviteState::Pending;
                return Ok(FetchOutcome::Pending);
            }
            InviteState::Rejected => {
                inv.state = InviteState::Rejected;
                return Ok(FetchOutcome::Rejected);
            }
            InviteState::Gone => return Ok(FetchOutcome::Expired),
            InviteState::Approved(sealed) => FetchOutcome::Sealed(sealed),
        };
        inv.terminal_at_ms = Some(now_ms);
        tracing::info!(
            invite_id = %inv.id,
            phone_fingerprint = %inv.fingerprint,
            "wfb_phone_invite_fetched"
        );
        Ok(outcome)
    }
}

/// The process-wide invite table.
static STORE: parking_lot::Mutex<InviteStore> = parking_lot::Mutex::new(InviteStore::new());

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
        InviteError::NotFound => {
            nested_detail(StatusCode::NOT_FOUND, "E_INVITE_NOT_FOUND", "no such invite")
        }
        InviteError::NotPending => nested_detail(
            StatusCode::CONFLICT,
            "E_INVITE_NOT_PENDING",
            "the invite has already been decided",
        ),
        InviteError::Expired => {
            nested_detail(StatusCode::GONE, "E_INVITE_EXPIRED", "the invite has expired")
        }
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
mod tests {
    use super::*;
    use ados_groundlink::pairing::{generate_keypair, open_from_peer, seal_to_peer_with};

    const T0: i64 = 1_800_000_000_000;

    async fn body_json(resp: Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn material() -> InviteMaterial {
        InviteMaterial {
            rx_key: (0u8..64).map(|i| i.wrapping_mul(7)).collect(),
            hop_key: ados_radio::hop::derive_pair_key(Some(&[0x5Au8; 64])),
            fleet_id: 7,
            slots: vec![(1, "drone-a1b2c3".into()), (2, "drone-d4e5f6".into())],
            channel: 149,
            rendezvous_channel: 149,
            mcs: 1,
            video_fec: (8, 12),
            aux_fec: (1, 2),
        }
    }

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn create(store: &mut InviteStore, phone_pub: &[u8], id: &str, now: i64) -> Response {
        let req = CreateInviteRequest {
            phone_pub_b64: b64(phone_pub),
            label: Some("Pilot phone".into()),
        };
        create_with(true, store, &req, id.into(), true, now)
    }

    fn b64d(v: &Value) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(v.as_str().unwrap())
            .unwrap()
    }

    #[tokio::test]
    async fn create_approve_fetch_once_then_gone() {
        let phone = generate_keypair();
        let mut store = InviteStore::new();
        let id = "00112233445566778899aabbccddeeff";

        let resp = create(&mut store, &phone.public, id, T0);
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body = body_json(resp).await;
        assert_eq!(body["invite_id"], id);
        assert_eq!(body["expires_at_ms"], T0 + INVITE_TTL_MS);
        assert_eq!(body["phone_fingerprint"], phone_fingerprint(&phone.public));

        let list = body_json(list_with(true, &mut store, T0 + 1)).await;
        assert_eq!(list["pending"].as_array().unwrap().len(), 1);
        assert_eq!(list["pending"][0]["label"], "Pilot phone");

        let resp = fetch_with(true, &mut store, id, T0 + 2);
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        assert_eq!(body_json(resp).await, json!({"state": "pending"}));

        let resp = approve_with(true, &mut store, id, || Ok(material()), T0 + 3);
        assert_eq!(resp.status(), StatusCode::OK);
        // Approved invites leave the pending list.
        let list = body_json(list_with(true, &mut store, T0 + 4)).await;
        assert!(list["pending"].as_array().unwrap().is_empty());

        let resp = fetch_with(true, &mut store, id, T0 + 5);
        assert_eq!(resp.status(), StatusCode::OK);
        let sealed = body_json(resp).await;
        let plaintext = open_from_peer(
            &phone.secret.to_bytes(),
            &b64d(&sealed["gs_eph_pub_b64"]),
            &b64d(&sealed["nonce_b64"]),
            &b64d(&sealed["ciphertext_b64"]),
            &invite_context(id),
        )
        .unwrap();
        assert_eq!(plaintext, bundle_plaintext(&material()));
        let bundle: Value = serde_json::from_slice(&plaintext).unwrap();
        assert_eq!(bundle["v"], 1);
        assert_eq!(bundle["fleet_id"], 7);
        assert_eq!(bundle["bandwidth_mhz"], 20);
        assert_eq!(bundle["ports"], json!({"video": 0, "control": 1, "aux_down": 2}));
        assert_eq!(bundle["slots"][1], json!({"slot": 2, "device_id": "drone-d4e5f6"}));
        assert_eq!(b64d(&bundle["rx_key_b64"]), material().rx_key);
        // The hop key is the derived HMAC key, never the raw shared key.
        assert_eq!(b64d(&bundle["hop_key_b64"]), material().hop_key.to_vec());
        assert_ne!(b64d(&bundle["hop_key_b64"]), vec![0x5Au8; 64]);

        let resp = fetch_with(true, &mut store, id, T0 + 6);
        assert_eq!(resp.status(), StatusCode::GONE);
        assert_eq!(body_json(resp).await, json!({"state": "expired"}));
    }

    #[tokio::test]
    async fn reject_answers_403_and_cannot_be_approved() {
        let phone = generate_keypair();
        let mut store = InviteStore::new();
        let id = "aa";
        create(&mut store, &phone.public, id, T0);
        assert_eq!(reject_with(true, &mut store, id, T0 + 1).status(), StatusCode::OK);
        let resp = fetch_with(true, &mut store, id, T0 + 2);
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert_eq!(body_json(resp).await, json!({"state": "rejected"}));
        let resp = approve_with(true, &mut store, id, || Ok(material()), T0 + 3);
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn pending_and_uncollected_invites_expire_to_410() {
        let phone = generate_keypair();
        let mut store = InviteStore::new();
        create(&mut store, &phone.public, "p", T0);
        create(&mut store, &phone.public, "a", T0);
        approve_with(true, &mut store, "a", || Ok(material()), T0 + 60_000);

        // The pending one lapses at T0 + TTL; the approved one has its own window.
        let resp = fetch_with(true, &mut store, "p", T0 + INVITE_TTL_MS);
        assert_eq!(resp.status(), StatusCode::GONE);
        let resp = approve_with(true, &mut store, "p", || Ok(material()), T0 + INVITE_TTL_MS);
        assert_eq!(resp.status(), StatusCode::GONE);
        let resp = fetch_with(true, &mut store, "a", T0 + 60_000 + INVITE_TTL_MS);
        assert_eq!(resp.status(), StatusCode::GONE);

        // Long after, the tombstones are forgotten.
        let resp = fetch_with(true, &mut store, "p", T0 + 10 * INVITE_TTL_MS + TOMBSTONE_MS);
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_fifth_pending_invite_is_refused() {
        let phone = generate_keypair();
        let mut store = InviteStore::new();
        for i in 0..MAX_PENDING {
            let resp = create(&mut store, &phone.public, &format!("id{i}"), T0);
            assert_eq!(resp.status(), StatusCode::CREATED);
        }
        let resp = create(&mut store, &phone.public, "id5", T0);
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            body_json(resp).await["detail"]["error"]["code"],
            "E_INVITE_LIMIT"
        );
        // Deciding one frees a place.
        reject_with(true, &mut store, "id0", T0 + 1);
        assert_eq!(
            create(&mut store, &phone.public, "id5", T0 + 2).status(),
            StatusCode::CREATED
        );
    }

    #[tokio::test]
    async fn bad_phone_key_and_unpaired_station_are_refused() {
        let mut store = InviteStore::new();
        let resp = create(&mut store, &[1u8; 31], "x", T0);
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let req = CreateInviteRequest {
            phone_pub_b64: b64(&[9u8; 32]),
            label: None,
        };
        let resp = create_with(true, &mut store, &req, "y".into(), false, T0);
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        assert_eq!(
            body_json(resp).await["detail"]["error"]["code"],
            "E_GS_NOT_PAIRED"
        );

        let phone = generate_keypair();
        create(&mut store, &phone.public, "z", T0);
        let resp = approve_with(true, &mut store, "z", || Err(MaterialError::NotPaired), T0 + 1);
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let resp = approve_with(
            true,
            &mut store,
            "z",
            || Err(MaterialError::HopKeyUnavailable),
            T0 + 1,
        );
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        // A failed approval leaves the invite pending.
        assert_eq!(fetch_with(true, &mut store, "z", T0 + 2).status(), StatusCode::ACCEPTED);
        assert_eq!(fetch_with(true, &mut store, "nope", T0 + 2).status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn drone_profile_gets_404_on_every_route() {
        let mut store = InviteStore::new();
        let req = CreateInviteRequest {
            phone_pub_b64: b64(&generate_keypair().public),
            label: None,
        };
        for resp in [
            create_with(false, &mut store, &req, "d".into(), true, T0),
            list_with(false, &mut store, T0),
            approve_with(false, &mut store, "d", || Ok(material()), T0),
            reject_with(false, &mut store, "d", T0),
            fetch_with(false, &mut store, "d", T0),
        ] {
            assert_eq!(resp.status(), StatusCode::NOT_FOUND);
            assert_eq!(
                body_json(resp).await,
                json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})
            );
        }
        assert!(store.invites.is_empty());
    }

    #[test]
    fn every_invite_route_is_relay_forbidden() {
        for path in [
            "/api/v1/ground-station/wfb/invite",
            "/api/v1/ground-station/wfb/invite/0011/approve",
            "/api/v1/ground-station/wfb/invite/0011/reject",
            "/api/v1/ground-station/wfb/invite/0011",
        ] {
            assert!(crate::auth::relay_forbidden(path), "{path} must be relay-forbidden");
        }
    }

    #[test]
    fn fingerprint_is_four_groups_of_uppercase_hex() {
        let fp = phone_fingerprint(&[0u8; 32]);
        // sha256 of 32 zero bytes begins 66687aadf862bd77.
        assert_eq!(fp, "6668-7AAD-F862-BD77");
    }

    /// The shared cross-language vector: a deterministic seal of the fixed
    /// material. Set `ADOS_WRITE_PHONE_INVITE_VECTOR=1` to (re)write the file;
    /// otherwise the committed file must match what the sealing code produces.
    #[test]
    fn phone_invite_vector_matches_the_committed_fixture() {
        let phone_secret: [u8; 32] = std::array::from_fn(|i| i as u8 + 1);
        let phone_public = x25519_public(phone_secret);
        let eph_secret: [u8; 32] = std::array::from_fn(|i| 0x40 + i as u8);
        let nonce: [u8; 12] = std::array::from_fn(|i| 0xA0 + i as u8);
        let invite_id = "0123456789abcdef0123456789abcdef";
        let context = invite_context(invite_id);
        let plaintext = bundle_plaintext(&material());
        let sealed =
            seal_to_peer_with(eph_secret, nonce, &phone_public, &context, &plaintext).unwrap();
        let opened = open_from_peer(
            &phone_secret,
            &sealed.eph_pub,
            &sealed.nonce,
            &sealed.ciphertext,
            &context,
        )
        .unwrap();
        assert_eq!(opened, plaintext);

        let vector = json!({
            "invite_id": invite_id,
            "phone_secret_key_hex": hex::encode(phone_secret),
            "phone_public_key_hex": hex::encode(phone_public),
            "gs_ephemeral_public_hex": hex::encode(sealed.eph_pub),
            "nonce_hex": hex::encode(sealed.nonce),
            "context_hex": hex::encode(&context),
            "ciphertext_hex": hex::encode(&sealed.ciphertext),
            "phone_fingerprint": phone_fingerprint(&phone_public),
            "expected_plaintext_json": String::from_utf8(plaintext).unwrap(),
        });
        let rendered = format!("{}\n", serde_json::to_string_pretty(&vector).unwrap());
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/phone-invite-vector.json");
        if std::env::var_os("ADOS_WRITE_PHONE_INVITE_VECTOR").is_some() {
            std::fs::write(&path, &rendered).unwrap();
        }
        let committed = std::fs::read_to_string(&path).unwrap();
        assert_eq!(committed.replace("\r\n", "\n"), rendered);
    }

    fn x25519_public(secret: [u8; 32]) -> [u8; 32] {
        // Derive through the same sealing primitive: sealing from `secret` to a
        // fixed peer reports `secret`'s public half as `eph_pub`.
        let peer = generate_keypair().public;
        seal_to_peer_with(secret, [0u8; 12], &peer, b"", b"")
            .unwrap()
            .eph_pub
    }
}
