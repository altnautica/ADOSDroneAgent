//! `POST /api/plugins/capability-token`: the agent-issued plugin capability
//! token.
//!
//! The token rides the plugin iframe's postMessage calls to the GCS bridge,
//! which verifies it with the same HMAC secret: HKDF-SHA256 over the pairing
//! key with the fixed salt `ados/plugin-capability-token/v1` and empty info, so
//! the agent and a paired GCS derive it independently. The token is
//! `b64url(claims).b64url(hmac)` (unpadded), the claims the canonical JSON of
//! `{agentId, expiresAt, grantedCapabilities, iss, operatorId, pluginId}`:
//! sorted keys, no whitespace, non-ASCII escaped, exactly as the Python mint
//! produced them, so either verifier accepts either issuer's tokens.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use ados_protocol::pairing_posture::Pairing;

use super::install::parse_body;
use super::{json_response, now_ms, respond, Refusal};
use crate::state::AppState;

type HmacSha256 = Hmac<Sha256>;

/// The HKDF salt, fixed by the token spec.
const HKDF_SALT_TOKEN_V1: &[u8] = b"ados/plugin-capability-token/v1";

/// The longest (and default) token lifetime.
const TOKEN_TTL_SECONDS_DEFAULT: i64 = 600;

/// HKDF-SHA256 (RFC 5869) to one 32-byte block with empty `info`: extract
/// `PRK = HMAC(salt, ikm)`, expand `T(1) = HMAC(PRK, 0x01)`.
fn derive_token_secret(pairing_key: &[u8]) -> [u8; 32] {
    let mut extract =
        HmacSha256::new_from_slice(HKDF_SALT_TOKEN_V1).expect("HMAC accepts any key length");
    extract.update(pairing_key);
    let prk = extract.finalize().into_bytes();
    let mut expand = HmacSha256::new_from_slice(&prk).expect("HMAC accepts any key length");
    expand.update(&[0x01]);
    expand.finalize().into_bytes().into()
}

/// Escape every non-ASCII character of compact JSON as `\uXXXX` (UTF-16, so a
/// supplementary character becomes a surrogate pair), which is what Python's
/// default `ensure_ascii` produces. Non-ASCII only ever occurs inside strings.
fn ensure_ascii(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    for c in json.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut units = [0u16; 2];
            for unit in c.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    out
}

/// The signed claims, fields in sorted-key order.
#[derive(Serialize)]
struct Claims<'a> {
    #[serde(rename = "agentId")]
    agent_id: &'a str,
    #[serde(rename = "expiresAt")]
    expires_at: i64,
    #[serde(rename = "grantedCapabilities")]
    granted_capabilities: &'a [String],
    iss: &'a str,
    #[serde(rename = "operatorId")]
    operator_id: &'a str,
    #[serde(rename = "pluginId")]
    plugin_id: &'a str,
}

/// Mint a token. `granted` is sorted and de-duplicated here.
pub(crate) fn mint_token(
    pairing_key: &str,
    plugin_id: &str,
    agent_id: &str,
    operator_id: &str,
    granted: &[String],
    expires_at_ms: i64,
) -> String {
    let mut granted: Vec<String> = granted.to_vec();
    granted.sort();
    granted.dedup();
    let issuer = format!("agent:{agent_id}");
    let claims = Claims {
        agent_id,
        expires_at: expires_at_ms,
        granted_capabilities: &granted,
        iss: &issuer,
        operator_id,
        plugin_id,
    };
    let blob = ensure_ascii(&serde_json::to_string(&claims).expect("claims serialize"));
    let mut mac = HmacSha256::new_from_slice(&derive_token_secret(pairing_key.as_bytes()))
        .expect("HMAC accepts any key length");
    mac.update(blob.as_bytes());
    let sig = mac.finalize().into_bytes();
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    format!("{}.{}", b64.encode(blob.as_bytes()), b64.encode(sig))
}

#[derive(Deserialize)]
struct TokenRequest {
    plugin_id: String,
    #[serde(default)]
    operator_id: Option<String>,
    #[serde(default)]
    ttl_seconds: Option<i64>,
}

/// The mint response, in the Python route's field order.
#[derive(Serialize)]
struct TokenResponse {
    ok: bool,
    token: String,
    #[serde(rename = "expiresAt")]
    expires_at: i64,
    issuer: String,
    #[serde(rename = "grantedCapabilities")]
    granted_capabilities: Vec<String>,
}

/// `POST /api/plugins/capability-token`: `{plugin_id, operator_id?,
/// ttl_seconds? (1..=600)}` → `{ok, token, expiresAt, issuer,
/// grantedCapabilities}`. An unpaired agent has no key to derive from
/// (`11 not_paired`, 409); an unknown plugin is `14 not_found`.
pub async fn mint_capability_token(State(state): State<AppState>, body: Bytes) -> Response {
    let req: TokenRequest = match parse_body(&body) {
        Ok(r) => r,
        Err(bad) => return bad.into_response(),
    };
    if let Some(ttl) = req.ttl_seconds {
        if !(1..=TOKEN_TTL_SECONDS_DEFAULT).contains(&ttl) {
            return crate::routes::detail(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("ttl_seconds must be between 1 and {TOKEN_TTL_SECONDS_DEFAULT}"),
            );
        }
    }
    let Pairing::Paired(pairing_key) = state.pairing.current() else {
        return respond(Err(Refusal::new(
            11,
            "not_paired",
            "agent must be paired to mint capability tokens",
            StatusCode::CONFLICT,
        )));
    };
    let plugin_id = req.plugin_id.clone();
    let granted = state
        .plugins
        .read(move |sup| {
            let install = sup
                .find_install(&plugin_id)
                .ok_or_else(|| Refusal::not_installed(&plugin_id))?;
            Ok(install
                .permissions
                .iter()
                .filter(|(_, g)| g.granted)
                .map(|(id, _)| id.clone())
                .collect::<Vec<String>>())
        })
        .await;
    let granted = match granted {
        Ok(g) => g,
        Err(r) => return respond(Err(r)),
    };
    let agent_id = crate::config::PairingConfig::load_from(&state.pairing_paths.config)
        .agent
        .device_id;
    let operator_id = req
        .operator_id
        .filter(|o| !o.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    let expires_at = now_ms() + req.ttl_seconds.unwrap_or(TOKEN_TTL_SECONDS_DEFAULT) * 1000;
    let token = mint_token(
        &pairing_key,
        &req.plugin_id,
        &agent_id,
        &operator_id,
        &granted,
        expires_at,
    );
    let mut granted = granted;
    granted.sort();
    granted.dedup();
    json_response(
        StatusCode::OK,
        &TokenResponse {
            ok: true,
            token,
            expires_at,
            issuer: format!("agent:{agent_id}"),
            granted_capabilities: granted,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A token minted by the Python issuer (`mint_agent_capability_token` with
    /// these inputs) reproduces byte for byte, so a GCS that verified the old
    /// issuer's tokens verifies these, and non-ASCII operator ids escape the way
    /// Python's `json.dumps` escapes them.
    #[test]
    fn token_matches_the_python_issuer() {
        let token = mint_token(
            "ados_test_pairing_key",
            "com.example.hello",
            "node-7",
            "op-\u{e9}\u{1f600}",
            &[
                "ui.slot.node-detail-tab".to_string(),
                "event.publish".to_string(),
            ],
            1_700_000_600_000,
        );
        assert_eq!(token, PYTHON_TOKEN);
    }

    /// Produced by the Python mint (`cryptography` HKDF, `json.dumps(sort_keys,
    /// separators=(",", ":"))`) over the inputs above.
    const PYTHON_TOKEN: &str = "eyJhZ2VudElkIjoibm9kZS03IiwiZXhwaXJlc0F0IjoxNzAwMDAwNjAwMDAwLCJncmFudGVkQ2FwYWJpbGl0aWVzIjpbImV2ZW50LnB1Ymxpc2giLCJ1aS5zbG90Lm5vZGUtZGV0YWlsLXRhYiJdLCJpc3MiOiJhZ2VudDpub2RlLTciLCJvcGVyYXRvcklkIjoib3AtXHUwMGU5XHVkODNkXHVkZTAwIiwicGx1Z2luSWQiOiJjb20uZXhhbXBsZS5oZWxsbyJ9.IxppSO6pjX8vjY_s6G4TZYu58P3bYdcUE-6EuLrlmY8";
}
