//! Claim and unpair routes.

use super::*;

/// The `POST /api/pairing/claim` request body: a single `user_id` string.
#[derive(serde::Deserialize)]
pub struct ClaimRequest {
    pub user_id: String,
}

/// `POST /api/pairing/claim` → `{api_key, device_id, name, mdns_host}` (all
/// strings); 409 `{"detail":"Already paired. Unpair first."}` when already
/// paired.
///
/// Writes `pairing.json` (the document the Python `PairingManager.claim` writes:
/// atomic, 0600, the four keys, code + pending key dropped) and returns the key.
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
    // The edge gate reads the same file through a short-TTL cache that still
    // holds "unpaired"; the caller's first keyed request follows at once.
    state.pairing.invalidate();

    let cfg = PairingConfig::load_from(&paths.config);
    let device_id = cfg.agent.device_id.clone();
    // The claim emits `agent.name` RAW (no "ADOS Agent" fallback — that
    // fallback is the /info route's, not the claim's).
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
/// Clears `pairing.json` (to the empty object `PairingManager.unpair` writes) and
/// mints a fresh pairing code. Requires a valid API key, enforced by the auth
/// middleware (this path is NOT in the public set).
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
    state.pairing.invalidate();

    // Mint + persist the fresh code after the unpair, the same rule the Python
    // `get_or_create_code()` applies. A code that could not be
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
