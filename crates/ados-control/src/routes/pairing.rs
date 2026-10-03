//! Pairing routes: the LAN node-identity probe + the local pairing handshake.
//!
//! These are the parity-critical routes: the GCS Add-a-Node flow probes
//! `/api/pairing/info`, then POSTs `/api/pairing/claim`, and stores the returned
//! key. The native surface must answer these byte-identically to the FastAPI
//! surface, down to every field name and the null-as-null shape, or pairing
//! breaks silently.
//!
//! - **`GET /api/pairing/info`** — the node-identity probe. Emits all 19 fields
//!   even when null (no field is ever omitted), reading device identity +
//!   profile off `/etc/ados/config.yaml`, the cloud-pair state off
//!   `pairing.json`, the radio-pair signal off the `/etc/ados/wfb` key files, the
//!   bind session off the `/run/ados/bind-state.json` sentinel, and the FC triple
//!   off the live state snapshot. Every optional read is fault-tolerant. The one
//!   exception is `pairing.json` itself: an unreadable or malformed file is a
//!   `503`, never the unpaired default, because "unpaired" is the state in which
//!   anyone on the LAN may claim a fresh key.
//! - **`GET /api/pairing/code`** — the live code while unpaired, regenerated
//!   once it is older than `pairing_store::CODE_TTL_SECONDS` (the same lifetime
//!   the Python writer applies); 409 when paired.
//! - **`POST /api/pairing/claim`** — claim the agent for a user. Writes
//!   `pairing.json` (mirroring `PairingManager.claim` exactly) and returns the
//!   key; 409 when already paired; 503 when the pairing file cannot be read.
//!   Never served to a request that crossed the radio relay.
//! - **`POST /api/pairing/unpair`** — clear pairing + mint a fresh code; 409 when
//!   not paired. Gated by the auth middleware (it is not in the public set). An
//!   unreadable pairing file is cleared too: the gate already restricts it to the
//!   on-box operator, and this is how that operator recovers the node.
//! - **`POST /api/pairing/accept`** — accept a code Mission Control generated:
//!   register this device against it at the pairing backend's
//!   `/pairing/register`, then claim locally under the key it registered.
//!   Always `200 {ok, error, message, owner_id, paired_at, device_id}` with the
//!   outcome in `ok`; never served to a request that crossed the radio relay.
//!
//! Every write happens under `pairing_store::lock_writers`, with the
//! already-paired check re-read inside the lock, so two concurrent claims cannot
//! both mint a key.
//!
//! The pairing code is withheld from a remote caller (anything relayed through a
//! proxy or tunnel, or a public-WAN host): `info` reports it as null and `code`
//! refuses. It is a claim credential for the device's own networks only.
//!
//! `mdns_host` is the RESOLVABLE reach name — the host name avahi actually
//! publishes (including a collision rename like `<hostname>-2.local`), else the
//! system hostname's first label under `.local`, resolved through
//! [`ados_protocol::reach::mdns_hostname`]. It is
//! deliberately NOT a constructed `ados-<6hex>.local`: nothing publishes an
//! A-record for that name, so a GCS that stores it as a node's canonical reach
//! stores a name that resolves nowhere. A host with no usable hostname has no
//! mDNS reach at all and the field is emitted as `""` rather than as a name this
//! node cannot prove. The `_ados._tcp` advert this daemon publishes at boot
//! (`crate::mdns`) uses the identical name as its SRV target, so the browse
//! record and the probe response name one host.

use axum::extract::{Extension, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use ados_protocol::pairing_posture::CallerClass;

use crate::config::PairingConfig;
use crate::pairing_store::{self, PairingDoc};
use crate::profile::current_profile_and_role_at;
use crate::routes::detail;
use crate::state::{AppState, PairingPaths};

/// What an operator is told when the pairing file exists but cannot be read or
/// parsed: the node refuses to act as unpaired (which would open the claim) and
/// names the recovery.
const UNREADABLE_MESSAGE: &str =
    "The pairing state on this device is unreadable. Unpair it on the device itself to recover.";

/// The `503` for a pairing file that cannot be read or parsed.
fn pairing_unreadable(reason: &str) -> Response {
    tracing::error!(reason, "pairing_state_unreadable");
    detail(StatusCode::SERVICE_UNAVAILABLE, UNREADABLE_MESSAGE)
}

/// Whether the caller may see the pairing code: anyone on the device's own
/// networks, never a remote caller. A request with no caller class (never
/// produced by the edges) is treated as remote.
fn may_see_code(caller: Option<Extension<CallerClass>>) -> bool {
    !matches!(
        caller.map_or(CallerClass::Remote, |Extension(c)| c),
        CallerClass::Remote
    )
}

/// `GET /api/pairing/info` → the 19-field node-identity probe.
///
/// Doubles as the Mission Control "probe" endpoint when an operator pastes a
/// hostname into Add-a-Node. Every field is emitted even when null (the GCS keys
/// off exact field presence), so `bind_state` and `radio` serialize as JSON
/// `null`, never omitted. Each underlying read is guarded so a partially
/// configured agent answers 200 with a usable shape rather than 500.
pub async fn get_pairing_info(
    State(state): State<AppState>,
    caller: Option<Extension<CallerClass>>,
) -> Response {
    let paths = &state.pairing_paths;

    // Device identity + profile, read live off the config (mirroring the FastAPI
    // route's read of the live runtime config).
    let cfg = PairingConfig::load_from(&paths.config);
    let device_id = cfg.agent.device_id.clone();
    // The FastAPI route falls back to "ADOS Agent" when the config name is empty
    // (`name or "ADOS Agent"`); the config default is "my-drone", so a configured
    // agent carries a real name here.
    let name = if cfg.agent.name.is_empty() {
        "ADOS Agent".to_string()
    } else {
        cfg.agent.name.clone()
    };
    let (profile, role) =
        current_profile_and_role_at(&cfg.agent.profile, &paths.profile_conf, &paths.mesh_role);
    let radio_peer_device_id = cfg.radio_peer_device_id();

    // The name this host actually answers to. Empty when the host has no
    // usable hostname: the GCS falls back to the IPv4 it just reached us on,
    // which is a proven reach, where a constructed name is not.
    let mdns_host = ados_protocol::reach::mdns_hostname().unwrap_or_default();

    // Cloud-pair state off pairing.json. Absent is unpaired; unreadable is not.
    let doc = match PairingDoc::read(&paths.pairing_json) {
        Ok(doc) => doc,
        Err(reason) => return pairing_unreadable(&reason),
    };

    // Radio-pair signal: the same predicate `GET /api/wfb/pair` answers from —
    // this role's own key file, exactly 64 bytes, with a readable fingerprint.
    let radio_paired = crate::wfb_pair_state::paired_key_fingerprint(
        &paths.wfb_key_dir,
        crate::wfb_pair_state::bind_role_for(&profile),
    )
    .is_some();

    // The folded bind-session snapshot from the cross-process sentinel.
    let bind_state = read_bind_state(paths);

    // FC presence from the live state snapshot's runtime extras.
    let (fc_connected, fc_port, fc_baud) = fc_from_snapshot(state.state.snapshot().as_ref());

    // The same live code `/code` serves. One that cannot be minted or persisted
    // is reported as absent here rather than failing the whole identity probe;
    // `/code` says why.
    let pairing_code = if doc.is_paired() {
        None
    } else {
        code_for_caller(paths, caller).await
    };

    Json(json!({
        "device_id": device_id,
        "name": name,
        "version": state.agent_version(),
        // Board is HAL-detected at runtime in the Python (the FastAPI route reads
        // `app.board_name`); the native surface has no in-process HAL-detect port,
        // so it reads the `name` field live off the board sidecar
        // (`/run/ados/board.json`) the detector persists — the same on-disk source
        // the status route's board block reads. Defaults to "unknown" when the
        // sidecar is absent (a fresh boot before the first status write).
        "board": crate::state::board_name(&state.board_path),
        "paired": doc.is_paired(),
        "radio_paired": radio_paired,
        "radio_peer_device_id": radio_peer_device_id,
        "pairing_code": pairing_code,
        "owner_id": doc.info_owner_id(),
        "paired_at": doc.info_paired_at(),
        "mdns_host": mdns_host,
        "profile": profile,
        "role": role,
        // Native-vs-packaged badge. The native surface has no in-process port of
        // the Python compute_runtime_mode(profile), so the Python API writes the
        // computed value to the `runtime-mode` sidecar at startup and this reads it
        // live (then the ADOS_RUNTIME_MODE env, then "packaged"). Defaults to
        // "packaged" when neither is present, the correct value for any agent that
        // has not cut over.
        "runtime_mode": crate::state::runtime_mode(),
        "bind_state": bind_state,
        // Reserved for a future in-process radio reader; null today, exactly as
        // the FastAPI route emits (the GCS falls back to radio_paired).
        "radio": Value::Null,
        "fc_connected": fc_connected,
        "fc_port": fc_port,
        "fc_baud": fc_baud,
    }))
    .into_response()
}

/// `GET /api/pairing/code` → `{"code": <code>}` while unpaired; 409
/// `{"detail":"Already paired"}` while paired.
///
/// Returns the persisted code while it is live, and mints then persists a new
/// one when there is none or it has outlived `pairing_store::CODE_TTL_SECONDS`,
/// the same rule the Python `get_or_create_code` applies, so every surface shows
/// one code. Paired agents 409.
pub async fn get_pairing_code(
    State(state): State<AppState>,
    caller: Option<Extension<CallerClass>>,
) -> Response {
    if !may_see_code(caller) {
        return detail(
            StatusCode::FORBIDDEN,
            "The pairing code is only served on the device's own networks.",
        );
    }
    match live_code(&state.pairing_paths).await {
        Ok(Some(code)) => (StatusCode::OK, Json(json!({ "code": code }))).into_response(),
        Ok(None) => detail(StatusCode::CONFLICT, "Already paired"),
        Err(CodeError::Unreadable(reason)) => pairing_unreadable(&reason),
        // A code that could not be persisted is refused: an in-memory one is
        // not what the device holds and would change on every call.
        Err(CodeError::Persist(e)) => {
            tracing::warn!(error = %e, "pairing code persist failed");
            detail(
                StatusCode::SERVICE_UNAVAILABLE,
                "Failed to persist a pairing code",
            )
        }
    }
}

/// Why no live code could be produced.
enum CodeError {
    Unreadable(String),
    Persist(std::io::Error),
}

/// The live pairing code of an unpaired node, minted under the writer lock
/// when absent or expired; `None` when paired.
async fn live_code(paths: &PairingPaths) -> Result<Option<String>, CodeError> {
    let now = now_unix_seconds();
    let doc = PairingDoc::read(&paths.pairing_json).map_err(CodeError::Unreadable)?;
    if doc.is_paired() {
        return Ok(None);
    }
    // The code and the pending key travel together; a live code with no key
    // beside it still needs a write.
    let has_pending_key = doc
        .pending_api_key
        .as_deref()
        .is_some_and(|k| !k.is_empty());
    if let Some(code) = doc.live_code(now).filter(|_| has_pending_key) {
        return Ok(Some(code));
    }
    let _lock = pairing_store::lock_writers(&paths.pairing_json)
        .await
        .map_err(CodeError::Persist)?;
    // Re-read under the lock: another writer may have paired the node or
    // minted a code meanwhile, and either must win over a second mint.
    let doc = PairingDoc::read(&paths.pairing_json).map_err(CodeError::Unreadable)?;
    if doc.is_paired() {
        return Ok(None);
    }
    pairing_store::ensure_code(&paths.pairing_json, now)
        .map(Some)
        .map_err(CodeError::Persist)
}

/// The pairing code `caller` may be shown: the live code of an unpaired node,
/// for a caller on the device's own networks. `None` for a remote caller, a
/// paired node, or a code that could not be minted or persisted (`/code` says
/// why), so a read that merely displays the code never fails on it.
pub(crate) async fn code_for_caller(
    paths: &PairingPaths,
    caller: Option<Extension<CallerClass>>,
) -> Option<String> {
    if !may_see_code(caller) {
        return None;
    }
    live_code(paths).await.ok().flatten()
}

/// The `POST /api/pairing/claim` request body: a single `user_id` string.
#[derive(serde::Deserialize)]
pub struct ClaimRequest {
    pub user_id: String,
}

/// `POST /api/pairing/claim` → `{api_key, device_id, name, mdns_host}` (all
/// strings); 409 `{"detail":"Already paired. Unpair first."}` when already
/// paired.
///
/// Writes `pairing.json` (mirroring `PairingManager.claim` exactly: atomic,
/// 0600, the four Python keys, code + pending key dropped) and returns the key.
/// No credential required and only works while unpaired. Being on the device's
/// own networks is the gate: the edge refuses a remote caller (a public-WAN
/// host, or anything relayed through a proxy or tunnel) before this handler
/// runs — see `auth::unpaired_decision`.
///
/// A request that crossed the radio relay is refused here as well as at the
/// edge. A drone paired only by radio is unpaired on its LAN side, so a claim
/// arriving over the relay would hand the master LAN key to whoever drives the
/// relay, to keep after leaving radio range.
pub async fn claim_pairing(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ClaimRequest>,
) -> Response {
    if headers.contains_key(crate::auth::RELAYED_HEADER) {
        tracing::warn!("pairing_claim_relayed_refused");
        return detail(
            StatusCode::FORBIDDEN,
            "A pairing claim cannot be made over the radio relay.",
        );
    }
    let paths = &state.pairing_paths;
    // Read, check and write under the writer lock, so two concurrent claims
    // cannot each see an unpaired node and mint their own key.
    let _lock = match pairing_store::lock_writers(&paths.pairing_json).await {
        Ok(lock) => lock,
        Err(e) => {
            tracing::error!(error = %e, "pairing claim lock failed");
            return detail(
                StatusCode::SERVICE_UNAVAILABLE,
                "The pairing state is busy. Retry shortly.",
            );
        }
    };
    let doc = match PairingDoc::read(&paths.pairing_json) {
        Ok(doc) => doc,
        Err(reason) => return pairing_unreadable(&reason),
    };
    if doc.is_paired() {
        return detail(StatusCode::CONFLICT, "Already paired. Unpair first.");
    }

    let outcome = match pairing_store::claim(&paths.pairing_json, &req.user_id, now_unix_seconds())
    {
        Ok(o) => o,
        // Fail closed: a getrandom failure while minting a fresh key 500s rather
        // than emitting a predictable key (distinct message from the persist
        // failure so the logs tell an entropy fault from a disk fault).
        Err(pairing_store::ClaimError::KeyGen(e)) => {
            tracing::error!(error = %e, "pairing claim key mint failed");
            return detail(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to mint pairing key",
            );
        }
        Err(pairing_store::ClaimError::Unreadable(reason)) => {
            return pairing_unreadable(&reason);
        }
        Err(pairing_store::ClaimError::Persist(e)) => {
            tracing::error!(error = %e, "pairing claim persist failed");
            return detail(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to persist pairing: {e}"),
            );
        }
    };

    let cfg = PairingConfig::load_from(&paths.config);
    let device_id = cfg.agent.device_id.clone();
    // The FastAPI claim emits `app.config.agent.name` RAW (no "ADOS Agent"
    // fallback — that fallback is the /info route's, not the claim's).
    let name = cfg.agent.name.clone();
    // The GCS persists this as the node's canonical reach and six consumers
    // prefer it over the IPv4 they just proved. So it must be the name this
    // host answers to, identical to the one `/api/pairing/info` reported and
    // the one the `_ados._tcp` advert targets.
    let mdns_host = ados_protocol::reach::mdns_hostname().unwrap_or_default();

    // The `_ados._tcp` advert's `paired` TXT is refreshed by the same daemon
    // that publishes it (`crate::mdns`), which re-reads pairing.json on a fixed
    // cadence rather than being poked from here — a claim that lands while the
    // advert thread is mid-publish must not be able to wedge the write path
    // the operator is waiting on.

    (
        StatusCode::OK,
        Json(json!({
            "api_key": outcome.api_key,
            "device_id": device_id,
            "name": name,
            "mdns_host": mdns_host,
        })),
    )
        .into_response()
}

/// `POST /api/pairing/unpair` → `{"status":"unpaired","new_code":<code>}`; 409
/// `{"detail":"Not paired"}` when not paired.
///
/// Clears `pairing.json` (mirroring `PairingManager.unpair` → empty object) and
/// mints a fresh pairing code. Requires a valid API key, enforced by the auth
/// middleware (this path is NOT in the public set), matching the FastAPI route.
/// The `_ados._tcp` advert picks the change up on its own refresh cadence, as
/// it does for a claim.
pub async fn unpair(State(state): State<AppState>) -> Response {
    let paths = &state.pairing_paths;
    let _lock = match pairing_store::lock_writers(&paths.pairing_json).await {
        Ok(lock) => lock,
        Err(e) => {
            tracing::error!(error = %e, "pairing unpair lock failed");
            return detail(
                StatusCode::SERVICE_UNAVAILABLE,
                "The pairing state is busy. Retry shortly.",
            );
        }
    };
    // An unreadable file is cleared as well: only the on-box operator reaches
    // this while it is unreadable (no key can match), and it is their recovery.
    if let Ok(doc) = PairingDoc::read(&paths.pairing_json) {
        if !doc.is_paired() {
            return detail(StatusCode::CONFLICT, "Not paired");
        }
    }

    if let Err(e) = pairing_store::unpair(&paths.pairing_json, &paths.relay_secret) {
        tracing::error!(error = %e, "pairing unpair persist failed");
        return detail(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to clear pairing: {e}"),
        );
    }

    // Mint + persist the fresh code, mirroring the FastAPI route's
    // `get_or_create_code()` after the unpair. A code that could not be
    // persisted is refused, as `/code` refuses it: an in-memory one is not what
    // the device holds. The node is already unpaired, so `/code` mints one once
    // the fault clears.
    let new_code = match pairing_store::write_new_code(&paths.pairing_json, now_unix_seconds()) {
        Ok(code) => code,
        Err(e) => {
            tracing::warn!(error = %e, "new pairing code persist failed after unpair");
            return detail(
                StatusCode::SERVICE_UNAVAILABLE,
                "Pairing was cleared, but a new pairing code could not be persisted. Read /api/pairing/code to retry.",
            );
        }
    };

    (
        StatusCode::OK,
        Json(json!({ "status": "unpaired", "new_code": new_code })),
    )
        .into_response()
}

/// The `POST /api/pairing/accept` request body: the code Mission Control
/// generated.
#[derive(serde::Deserialize)]
pub struct AcceptCodeRequest {
    pub code: String,
}

/// The `POST /api/pairing/accept` reply. Every field is always present, null
/// where it does not apply: `ok` carries the outcome, `error` + `message` a
/// failure, the other three a success.
#[derive(serde::Serialize, Default)]
struct AcceptCodeResponse {
    ok: bool,
    error: Option<String>,
    message: Option<String>,
    owner_id: Option<String>,
    paired_at: Option<f64>,
    device_id: Option<String>,
}

impl AcceptCodeResponse {
    fn failure(error: &str, message: impl Into<String>) -> Self {
        tracing::warn!(error, "pairing_accept_failed");
        Self {
            ok: false,
            error: Some(error.to_string()),
            message: Some(message.into()),
            ..Self::default()
        }
    }
}

/// How long the backend register call may take.
const ACCEPT_REGISTER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// `POST /api/pairing/accept` → `{ok, error, message, owner_id, paired_at,
/// device_id}`, always 200.
///
/// Lets an operator pre-allocate a code on the Mission Control side and type it
/// into this device, instead of typing the device's code into Mission Control.
/// The device registers itself against that code at the pairing backend with a
/// freshly minted key; when the backend confirms the match it claims locally
/// under that same key, so the key the backend froze is the key the device
/// validates. Every failure (a malformed code, an already-paired node, no
/// backend configured, an unreachable or refusing backend, an unknown code) is
/// `ok: false` with a stable `error` code and an operator-facing `message`.
///
/// A request that crossed the radio relay is refused, as `/claim` refuses one:
/// accepting a code mints the master LAN key.
pub async fn accept_pairing_code(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<AcceptCodeRequest>,
) -> Response {
    if headers.contains_key(crate::auth::RELAYED_HEADER) {
        tracing::warn!("pairing_accept_relayed_refused");
        return detail(
            StatusCode::FORBIDDEN,
            "A pairing code cannot be accepted over the radio relay.",
        );
    }
    Json(accept_external_code(&state, &req.code).await).into_response()
}

async fn accept_external_code(state: &AppState, code: &str) -> AcceptCodeResponse {
    let cleaned: String = code
        .to_uppercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect();
    if cleaned.chars().count() != pairing_store::CODE_LENGTH {
        return AcceptCodeResponse::failure("invalid_code", "Pairing code must be 6 characters.");
    }

    let paths = &state.pairing_paths;
    let config = match crate::routes::config_rw::effective_config(&paths.config) {
        Ok(config) => config,
        Err(reason) => {
            return AcceptCodeResponse::failure(
                "agent_not_ready",
                format!("The agent config could not be read: {reason}"),
            );
        }
    };
    match PairingDoc::read(&paths.pairing_json) {
        Ok(doc) if doc.is_paired() => {
            return AcceptCodeResponse::failure(
                "already_paired",
                "This device is already paired. Unpair first.",
            );
        }
        Ok(_) => {}
        Err(reason) => {
            tracing::error!(reason = %reason, "pairing_state_unreadable");
            return AcceptCodeResponse::failure("agent_not_ready", UNREADABLE_MESSAGE);
        }
    }

    // The self-hosted posture registers at the operator's deployment; every
    // other posture at the managed backend.
    let mode = config
        .pointer("/server/mode")
        .and_then(Value::as_str)
        .unwrap_or("");
    let url_pointer = if mode == "self_hosted" {
        "/server/self_hosted/url"
    } else {
        "/server/cloud/url"
    };
    let site = convex_site_url(
        config
            .pointer(url_pointer)
            .and_then(Value::as_str)
            .unwrap_or(""),
    );
    if site.is_empty() {
        return AcceptCodeResponse::failure(
            "no_backend",
            "No cloud backend is configured. This agent is in local mode — \
             pair it directly from Mission Control by hostname or IP instead.",
        );
    }

    let identity = PairingConfig::load_from(&paths.config);
    let device_id = identity.agent.device_id.clone();
    tracing::info!(convex_url = %site, mode, device_id = %device_id, "pairing_accept_attempt");

    let api_key = match pairing_store::generate_api_key() {
        Ok(key) => key,
        Err(e) => {
            tracing::error!(error = %e, "pairing accept key mint failed");
            return AcceptCodeResponse::failure("agent_not_ready", "Failed to mint pairing key");
        }
    };
    let board = crate::routes::status::read_board(&state.board_path);
    let body = json!({
        "deviceId": device_id,
        "pairingCode": cleaned,
        "apiKey": api_key,
        "name": identity.agent.name,
        "version": state.agent_version(),
        "board": crate::state::board_name(&state.board_path),
        "tier": board.get("tier").and_then(Value::as_i64).unwrap_or(0),
        "mdnsHost": ados_protocol::reach::mdns_hostname().unwrap_or_default(),
        "localIp": "",
    });

    let (status, reply) = match post_register(&format!("{site}/pairing/register"), &body).await {
        Ok(answer) => answer,
        Err(e) => {
            return AcceptCodeResponse::failure(
                "network",
                format!("Could not reach the cloud backend: {e}"),
            );
        }
    };
    if status != 200 {
        return AcceptCodeResponse::failure("backend_error", format!("Backend returned {status}."));
    }
    let Ok(result) = serde_json::from_slice::<Value>(&reply) else {
        return AcceptCodeResponse::failure("bad_response", "Backend response was not JSON.");
    };
    if let Some(err) = result.get("error").filter(|e| truthy(e)) {
        let err = err
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| err.to_string());
        let message = match err.as_str() {
            "device_pending_with_different_code" => {
                "This device is already pending a different code. Unpair first.".to_string()
            }
            "pairing_code_expired" => {
                "The pairing code has expired. Generate a fresh one.".to_string()
            }
            other => other.to_string(),
        };
        return AcceptCodeResponse::failure(&err, message);
    }
    let matched = ["autoMatched", "alreadyClaimed"]
        .iter()
        .any(|k| result.get(k).is_some_and(truthy));
    if !matched {
        return AcceptCodeResponse::failure(
            "code_unknown",
            "No Mission Control session is waiting on that code yet. \
             Ask Mission Control to generate a code and try again.",
        );
    }
    let owner_id = ["userId", "ownerId"]
        .iter()
        .find_map(|k| {
            result.get(k).filter(|v| truthy(v)).map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| v.to_string())
            })
        })
        .unwrap_or_else(|| "cloud".to_string());

    // Persist under the writer lock, re-checking the pair state inside it: a
    // local claim that landed while the backend call was in flight wins.
    let _lock = match pairing_store::lock_writers(&paths.pairing_json).await {
        Ok(lock) => lock,
        Err(e) => {
            tracing::error!(error = %e, "pairing accept lock failed");
            return AcceptCodeResponse::failure(
                "agent_not_ready",
                "The pairing state is busy. Retry shortly.",
            );
        }
    };
    match PairingDoc::read(&paths.pairing_json) {
        Ok(doc) if doc.is_paired() => {
            return AcceptCodeResponse::failure(
                "already_paired",
                "This device is already paired. Unpair first.",
            );
        }
        Ok(_) => {}
        Err(reason) => {
            tracing::error!(reason = %reason, "pairing_state_unreadable");
            return AcceptCodeResponse::failure("agent_not_ready", UNREADABLE_MESSAGE);
        }
    }
    let paired_at = now_unix_seconds();
    if let Err(e) =
        pairing_store::claim_with_key(&paths.pairing_json, &owner_id, &api_key, paired_at)
    {
        tracing::error!(error = %e, "pairing accept persist failed");
        let message = match e {
            pairing_store::ClaimError::Unreadable(_) => UNREADABLE_MESSAGE.to_string(),
            other => format!("Failed to persist pairing: {other}"),
        };
        return AcceptCodeResponse::failure("agent_not_ready", message);
    }
    tracing::info!(owner_id = %owner_id, device_id = %device_id, "pairing_accept_succeeded");
    AcceptCodeResponse {
        ok: true,
        owner_id: Some(owner_id),
        paired_at: Some(paired_at),
        device_id: Some(device_id),
        ..AcceptCodeResponse::default()
    }
}

/// POST the register body and return the status with the raw reply. Plain-HTTP
/// URLs (a self-hosted deployment on the LAN) are dialled as given; HTTPS
/// verifies against the bundled webpki roots.
async fn post_register(url: &str, body: &Value) -> Result<(u16, bytes::Bytes), reqwest::Error> {
    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("rustls accepts the default protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    let client = reqwest::Client::builder()
        .use_preconfigured_tls(tls)
        .timeout(ACCEPT_REGISTER_TIMEOUT)
        .build()?;
    let resp = client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body.to_string())
        .send()
        .await?;
    let status = resp.status().as_u16();
    Ok((status, resp.bytes().await?))
}

/// Normalize an operator-entered Convex URL toward the SITE (HTTP-actions)
/// origin where `/pairing/register` is served. A self-hosted backend on `:3210`
/// maps to its site on `:3211`, and the managed backend host to its `-site`
/// sibling. Any other URL (already a site origin, or one that cannot be
/// rewritten with confidence) is returned unchanged, minus trailing slashes.
fn convex_site_url(url: &str) -> String {
    let cleaned = url.trim().trim_end_matches('/');
    if cleaned.contains(":3210") {
        return cleaned.replace(":3210", ":3211");
    }
    if cleaned.contains("://convex.altnautica.com") {
        return cleaned.replace("://convex.altnautica.com", "://convex-site.altnautica.com");
    }
    cleaned.to_string()
}

/// JSON truthiness as the backend's reply is read: false, null, zero and empty
/// strings, arrays and objects are false.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

// --- helpers ---

/// Fold the WFB bind-session snapshot from the cross-process sentinel. Absent
/// file (no bind has run) or a sentinel with no `state` → `null`. Each field is
/// read by key with a missing field tolerated, mirroring the FastAPI
/// `.get()`-guarded fold. Only the six fields the FastAPI route folds are
/// emitted, each as JSON null when absent in the sentinel.
fn read_bind_state(paths: &PairingPaths) -> Value {
    let Ok(text) = std::fs::read_to_string(&paths.bind_state) else {
        return Value::Null;
    };
    let Ok(sess) = serde_json::from_str::<Value>(&text) else {
        return Value::Null;
    };
    let Some(obj) = sess.as_object() else {
        return Value::Null;
    };
    // Best-effort schema-drift signal (never reject): warn when the sentinel was
    // written by an agent with a different schema version, then read anyway. The
    // writer const lives in the supervisor crate, so compare against the shared
    // registry.
    let got = obj.get("version").and_then(Value::as_u64).unwrap_or(0) as u16;
    if let Some(ours) = ados_protocol::contracts::sidecar_version("bind-state") {
        ados_protocol::sidecar::check_sidecar_version("bind-state", got, ours);
    }
    // The FastAPI route only folds when `sess.get("state")` is truthy.
    let state_truthy = obj
        .get("state")
        .map(|v| !v.is_null() && v != &json!("") && v != &json!(false))
        .unwrap_or(false);
    if !state_truthy {
        return Value::Null;
    }
    json!({
        "state": obj.get("state").cloned().unwrap_or(Value::Null),
        "phase": obj.get("phase").cloned().unwrap_or(Value::Null),
        // `bool(sess.get("active", False))` → a missing/falsey active is false.
        "active": obj.get("active").and_then(Value::as_bool).unwrap_or(false),
        "error": obj.get("error").cloned().unwrap_or(Value::Null),
        "finished_at": obj.get("finished_at").cloned().unwrap_or(Value::Null),
        "fingerprint": obj.get("fingerprint").cloned().unwrap_or(Value::Null),
    })
}

/// Read the FC connection triple out of the live state snapshot's runtime extras.
/// Returns `(fc_connected, fc_port, fc_baud)` as JSON values. Mirrors the FastAPI
/// route's `fc_status()`: a connected FC reports a string port + int baud, an
/// absent / disconnected one reports `false` + JSON `null` + JSON `null` (the
/// pairing-info defaults are `None`, unlike the status route's `""`/`0`).
pub(crate) fn fc_from_snapshot(snapshot: Option<&Value>) -> (Value, Value, Value) {
    let obj = snapshot.and_then(Value::as_object);
    let connected = obj
        .and_then(|m| m.get("fc_connected"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // The FastAPI route reports `str(fc.port) if fc.port else None` and
    // `int(fc.baud) if fc.baud else None`: a missing, null, or falsey value → null.
    let port = obj
        .and_then(|m| m.get("fc_port"))
        .filter(|v| v.is_string() && !v.as_str().unwrap_or("").is_empty())
        .cloned()
        .unwrap_or(Value::Null);
    let baud = obj
        .and_then(|m| m.get("fc_baud"))
        .filter(|v| v.as_i64().map(|n| n != 0).unwrap_or(false))
        .cloned()
        .unwrap_or(Value::Null);
    (json!(connected), port, baud)
}

/// Wall-clock unix seconds (fractional), matching the Python `time.time()` the
/// claim/unpair/code writers stamp.
fn now_unix_seconds() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fc_triple_is_disconnected_with_nulls_when_the_snapshot_is_absent() {
        let (c, p, b) = fc_from_snapshot(None);
        assert_eq!(c, json!(false));
        // Pairing-info uses null (not the status route's "" / 0).
        assert_eq!(p, Value::Null);
        assert_eq!(b, Value::Null);
    }

    #[test]
    fn fc_triple_reads_a_connected_snapshot() {
        let snap = json!({
            "fc_connected": true,
            "fc_port": "/dev/ttyACM0",
            "fc_baud": 115200,
        });
        let (c, p, b) = fc_from_snapshot(Some(&snap));
        assert_eq!(c, json!(true));
        assert_eq!(p, json!("/dev/ttyACM0"));
        assert_eq!(b, json!(115200));
    }

    #[test]
    fn fc_triple_treats_empty_port_and_zero_baud_as_null() {
        let snap = json!({
            "fc_connected": false,
            "fc_port": "",
            "fc_baud": 0,
        });
        let (c, p, b) = fc_from_snapshot(Some(&snap));
        assert_eq!(c, json!(false));
        assert_eq!(p, Value::Null);
        assert_eq!(b, Value::Null);
    }

    #[test]
    fn bind_state_is_null_for_an_absent_sentinel() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        assert_eq!(read_bind_state(&paths), Value::Null);
    }

    #[test]
    fn bind_state_is_null_when_the_sentinel_has_no_state() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bind-state.json"), r#"{"phase":"x"}"#).unwrap();
        let paths = test_paths(dir.path());
        assert_eq!(read_bind_state(&paths), Value::Null);
    }

    #[test]
    fn bind_state_folds_the_six_fields_when_state_is_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("bind-state.json"),
            r#"{"state":"binding","phase":"key_transfer","active":true,"error":null,"finished_at":123.0,"fingerprint":"ab"}"#,
        )
        .unwrap();
        let paths = test_paths(dir.path());
        let bs = read_bind_state(&paths);
        let obj = bs.as_object().expect("bind_state object");
        let keys: std::collections::BTreeSet<_> = obj.keys().cloned().collect();
        let want: std::collections::BTreeSet<_> = [
            "state",
            "phase",
            "active",
            "error",
            "finished_at",
            "fingerprint",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        assert_eq!(keys, want, "bind_state folds exactly the six FastAPI keys");
        assert_eq!(bs["state"], json!("binding"));
        assert_eq!(bs["active"], json!(true));
        assert_eq!(bs["error"], Value::Null);
    }

    #[test]
    fn bind_state_missing_active_folds_to_false() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bind-state.json"), r#"{"state":"done"}"#).unwrap();
        let paths = test_paths(dir.path());
        let bs = read_bind_state(&paths);
        assert_eq!(bs["active"], json!(false));
        assert_eq!(bs["phase"], Value::Null);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_code_that_cannot_be_persisted_is_refused_not_served() {
        // No code on file and the pairing directory is read-only: the route must
        // not hand the operator a code the device does not hold.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let ro = dir.path().join("ro");
        std::fs::create_dir(&ro).unwrap();
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
        let state = AppState::new(
            std::sync::Arc::new(crate::auth::PairingState::with_path(
                ro.join("pairing.json"),
            )),
            crate::ipc::StateIpcClient::disconnected(),
            crate::ipc::MavlinkIpcClient::new(dir.path().join("absent-mavlink.sock")),
            crate::ipc::LogdQueryClient::new(dir.path().join("absent-logd.sock")),
            dir.path().join("board.json"),
            test_paths(&ro),
            std::sync::Arc::new(crate::dashboard_pin::DashboardPin::with_path(
                dir.path().join("dashboard-pin.json"),
            )),
            std::sync::Arc::new(crate::mcp::McpTokenStore::with_path(
                dir.path().join("mcp-token.json"),
            )),
        );
        let resp = get_pairing_code(State(state), Some(Extension(CallerClass::OnBox))).await;
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(!ro.join("pairing.json").exists());
    }

    fn test_state(dir: &std::path::Path) -> AppState {
        AppState::new(
            std::sync::Arc::new(crate::auth::PairingState::with_path(
                dir.join("pairing.json"),
            )),
            crate::ipc::StateIpcClient::disconnected(),
            crate::ipc::MavlinkIpcClient::new(dir.join("absent-mavlink.sock")),
            crate::ipc::LogdQueryClient::new(dir.join("absent-logd.sock")),
            dir.join("board.json"),
            test_paths(dir),
            std::sync::Arc::new(crate::dashboard_pin::DashboardPin::with_path(
                dir.join("dashboard-pin.json"),
            )),
            std::sync::Arc::new(crate::mcp::McpTokenStore::with_path(
                dir.join("mcp-token.json"),
            )),
        )
    }

    fn claim_body(user: &str) -> Json<ClaimRequest> {
        Json(ClaimRequest {
            user_id: user.to_string(),
        })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_claims_hand_out_one_key() {
        // Two browsers claiming at once used to both get 200 with different
        // keys; the loser's key then failed every request.
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let calls: Vec<_> = (0..8)
            .map(|i| {
                let state = state.clone();
                tokio::spawn(async move {
                    claim_pairing(State(state), HeaderMap::new(), claim_body(&format!("u{i}")))
                        .await
                        .status()
                })
            })
            .collect();
        let mut ok = 0;
        for call in calls {
            match call.await.unwrap() {
                StatusCode::OK => ok += 1,
                StatusCode::CONFLICT => {}
                other => panic!("unexpected {other}"),
            }
        }
        assert_eq!(ok, 1, "exactly one claim succeeds");
    }

    #[tokio::test]
    async fn a_relayed_claim_is_refused_even_on_an_unpaired_node() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let mut headers = HeaderMap::new();
        headers.insert(crate::auth::RELAYED_HEADER, "1".parse().unwrap());
        let resp = claim_pairing(State(state), headers, claim_body("radio")).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert!(
            !dir.path().join("pairing.json").exists(),
            "nothing was minted"
        );
    }

    #[tokio::test]
    async fn an_expired_code_is_replaced_and_a_live_one_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let pairing = dir.path().join("pairing.json");
        std::fs::write(
            &pairing,
            r#"{"pairing_code":"OLD234","code_created_at":1.0,"pending_api_key":"ados_k"}"#,
        )
        .unwrap();
        let caller = || Some(Extension(CallerClass::OnBox));
        let resp = get_pairing_code(State(state.clone()), caller()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let first = PairingDoc::read(&pairing).unwrap().pairing_code.unwrap();
        assert_ne!(first, "OLD234", "a day-old code is regenerated");
        assert_eq!(
            PairingDoc::read(&pairing)
                .unwrap()
                .pending_api_key
                .as_deref(),
            Some("ados_k"),
            "the pending key the beacon advertised survives a new code"
        );
        let _ = get_pairing_code(State(state), caller()).await;
        assert_eq!(
            PairingDoc::read(&pairing).unwrap().pairing_code.unwrap(),
            first,
            "a live code is stable"
        );
    }

    #[tokio::test]
    async fn a_live_code_without_a_pending_key_gets_one_and_keeps_the_code() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let pairing = dir.path().join("pairing.json");
        let fresh = now_unix_seconds();
        std::fs::write(
            &pairing,
            format!(r#"{{"pairing_code":"LIVE23","code_created_at":{fresh}}}"#),
        )
        .unwrap();
        let resp = get_pairing_code(State(state), Some(Extension(CallerClass::OnBox))).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let doc = PairingDoc::read(&pairing).unwrap();
        assert_eq!(doc.pairing_code.as_deref(), Some("LIVE23"));
        assert!(doc.pending_api_key.unwrap().starts_with("ados_"));
    }

    async fn accept_body(resp: Response) -> (StatusCode, Value) {
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    fn accept_req(code: &str) -> Json<AcceptCodeRequest> {
        Json(AcceptCodeRequest {
            code: code.to_string(),
        })
    }

    #[tokio::test]
    async fn a_malformed_accept_code_is_refused_with_every_field_and_no_write() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        // Separators are dropped before the length check, so a 5-character
        // code is refused however it is punctuated.
        let resp = accept_pairing_code(State(state), HeaderMap::new(), accept_req("ab-c 23")).await;
        let (status, body) = accept_body(resp).await;
        assert_eq!(status, StatusCode::OK, "the outcome travels in `ok`");
        assert_eq!(
            body,
            json!({
                "ok": false,
                "error": "invalid_code",
                "message": "Pairing code must be 6 characters.",
                "owner_id": null,
                "paired_at": null,
                "device_id": null,
            })
        );
        assert!(!dir.path().join("pairing.json").exists());
    }

    #[tokio::test]
    async fn a_relayed_accept_is_refused_before_anything_is_minted() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let mut headers = HeaderMap::new();
        headers.insert(crate::auth::RELAYED_HEADER, "1".parse().unwrap());
        let resp = accept_pairing_code(State(state), headers, accept_req("ABC234")).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert!(!dir.path().join("pairing.json").exists());
    }

    #[tokio::test]
    async fn a_paired_node_refuses_an_accept_and_keeps_its_key() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let pairing = dir.path().join("pairing.json");
        let before = r#"{"paired":true,"api_key":"ados_k","owner_id":"u1","paired_at":1.0}"#;
        std::fs::write(&pairing, before).unwrap();
        let resp = accept_pairing_code(State(state), HeaderMap::new(), accept_req("ABC234")).await;
        let (_status, body) = accept_body(resp).await;
        assert_eq!(body["ok"], json!(false));
        assert_eq!(body["error"], json!("already_paired"));
        assert_eq!(std::fs::read_to_string(&pairing).unwrap(), before);
    }

    #[test]
    fn a_backend_url_is_mapped_to_its_site_origin() {
        assert_eq!(
            convex_site_url("http://192.168.1.50:3210/"),
            "http://192.168.1.50:3211"
        );
        assert_eq!(
            convex_site_url("https://convex.altnautica.com"),
            "https://convex-site.altnautica.com"
        );
        assert_eq!(
            convex_site_url("https://convex-site.altnautica.com"),
            "https://convex-site.altnautica.com"
        );
        assert_eq!(convex_site_url("   "), "");
    }

    fn test_paths(dir: &std::path::Path) -> PairingPaths {
        PairingPaths {
            config: dir.join("config.yaml"),
            pairing_json: dir.join("pairing.json"),
            wfb_key_dir: dir.join("wfb"),
            bind_state: dir.join("bind-state.json"),
            profile_conf: dir.join("profile.conf"),
            mesh_role: dir.join("mesh-role"),
            relay_secret: dir.join("relay-peer-secret"),
        }
    }
}
