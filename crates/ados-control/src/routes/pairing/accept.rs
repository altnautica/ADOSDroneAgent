//! Accepting an externally issued pairing code.

use super::*;

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
pub(super) struct AcceptCodeResponse {
    pub(super) ok: bool,
    pub(super) error: Option<String>,
    pub(super) message: Option<String>,
    pub(super) owner_id: Option<String>,
    pub(super) paired_at: Option<f64>,
    pub(super) device_id: Option<String>,
}

impl AcceptCodeResponse {
    pub(super) fn failure(error: &str, message: impl Into<String>) -> Self {
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
pub(super) const ACCEPT_REGISTER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

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

pub(super) async fn accept_external_code(state: &AppState, code: &str) -> AcceptCodeResponse {
    let cleaned: String = code
        .to_uppercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect();
    if cleaned.chars().count() != pairing_store::CODE_LENGTH {
        return AcceptCodeResponse::failure("invalid_code", "Pairing code must be 6 characters.");
    }

    let paths = &state.pairing_paths;
    let config = match ados_config::schema::effective(&paths.config) {
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
    state.pairing.invalidate();
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
pub(super) async fn post_register(
    url: &str,
    body: &Value,
) -> Result<(u16, bytes::Bytes), reqwest::Error> {
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
pub(super) fn convex_site_url(url: &str) -> String {
    let cleaned = url.trim().trim_end_matches('/');
    if cleaned.contains(":3210") {
        return cleaned.replace(":3210", ":3211");
    }
    if cleaned.contains("://convex.altnautica.com") {
        return cleaned.replace("://convex.altnautica.com", "://convex-site.altnautica.com");
    }
    cleaned.to_string()
}
