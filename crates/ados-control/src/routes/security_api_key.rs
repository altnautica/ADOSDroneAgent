//! `POST /api/v1/security/api-key/rotate` and `DELETE /api/v1/security/api-key`:
//! the operator API key (`security.api.api_key`), replaced or cleared.
//!
//! The operator key is the second credential
//! [`crate::auth::PairingState::credential_valid`] accepts beside the pairing
//! key. Both routes write it through the shared config store
//! ([`crate::config_store::update_config`]), the writer `PUT /api/config`
//! uses, and the credential check reads that file through the live security
//! posture ([`crate::security_state::SecurityState`]). Once the write lands the
//! route drops the cached config snapshot
//! ([`crate::config::forget_config_snapshot`]), so the change takes effect on
//! the very next request, without a restart: the old key is refused and the
//! new one accepted at once.
//!
//! - **rotate** mints a key in the pairing key's format (`ados_` + url-safe
//!   base64, no padding, of 32 random bytes), stores it, and returns it once:
//!   `{ok: true, api_key, rotated_at}`, `rotated_at` an RFC 3339 UTC stamp. No
//!   read route returns it again (`GET /api/config` redacts it).
//! - **clear** removes the key and answers `{ok: true}`. The pairing key is
//!   untouched, so a paired client keeps working.
//!
//! Who may call them:
//!
//! - The normal edge gate applies (both paths are non-public): on-box, or a
//!   valid credential or dashboard session.
//! - An MCP token never reaches them: `crate::mcp::route_scope` maps neither
//!   path, and an unmapped write is denied to a token.
//! - A request that crossed the radio relay, or that arrived through a proxy or
//!   tunnel (a remote caller, which is how the cloud path reaches the node), is
//!   refused with 403 in the handler. A key minted there would outlive the link
//!   that carried the request.
//!
//! The structured log events `security.api_key_rotated` and
//! `security.api_key_cleared` record each change; neither carries the key.

use axum::extract::{Extension, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;
use serde_norway::Value as Yaml;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use ados_protocol::pairing_posture::CallerClass;

use crate::auth::RELAYED_HEADER;
use crate::config::forget_config_snapshot;
use crate::config_store::{section_path, update_config, ConfigWriteError};
use crate::pairing_store::generate_api_key;
use crate::routes::detail;
use crate::state::AppState;

/// The config sections holding the operator key, and its leaf.
const KEY_SECTIONS: [&str; 2] = ["security", "api"];
const KEY_LEAF: &str = "api_key";

/// The refusal for a caller that may not change the operator key, or `None`
/// when it may. A request with no caller class (one the edge did not stamp) is
/// treated as remote, so the gate fails closed.
fn refuse_off_link(
    caller: Option<Extension<CallerClass>>,
    headers: &HeaderMap,
    action: &'static str,
) -> Option<Response> {
    let caller = caller.map_or(CallerClass::Remote, |Extension(c)| c);
    let relayed = headers.contains_key(RELAYED_HEADER);
    if !relayed && caller != CallerClass::Remote {
        return None;
    }
    tracing::warn!(?caller, relayed, action, "security.api_key_change_refused");
    Some(detail(
        StatusCode::FORBIDDEN,
        "The operator API key can only be changed on the device or from its own network.",
    ))
}

/// The 500 for a write that did not land. Nothing was written.
fn write_failed(error: &ConfigWriteError, action: &'static str) -> Response {
    tracing::warn!(error = %error, action, "security.api_key_write_failed");
    detail(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("The operator API key was not changed: {error}"),
    )
}

/// `POST /api/v1/security/api-key/rotate` → `{ok: true, api_key, rotated_at}`.
pub async fn rotate_api_key(
    State(state): State<AppState>,
    caller: Option<Extension<CallerClass>>,
    headers: HeaderMap,
) -> Response {
    if let Some(refused) = refuse_off_link(caller, &headers, "rotate") {
        return refused;
    }
    // Fails closed: a key over a buffer the RNG did not fill would be guessable.
    let key = match generate_api_key() {
        Ok(key) => key,
        Err(e) => {
            tracing::error!(error = %e, "security.api_key_generate_failed");
            return detail(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not generate an API key.",
            );
        }
    };
    let stored = key.clone();
    let written = update_config(&state.pairing_paths.config, move |root| {
        section_path(root, &KEY_SECTIONS)
            .insert(Yaml::String(KEY_LEAF.into()), Yaml::String(stored));
        Ok(())
    });
    if let Err(e) = written {
        return write_failed(&e, "rotate");
    }
    forget_config_snapshot(&state.pairing_paths.config);
    let rotated_at = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default();
    tracing::info!(rotated_at = %rotated_at, "security.api_key_rotated");
    Json(json!({ "ok": true, "api_key": key, "rotated_at": rotated_at })).into_response()
}

/// `DELETE /api/v1/security/api-key` → `{ok: true}`. Clearing a key that is not
/// set writes nothing and still answers `ok`.
pub async fn clear_api_key(
    State(state): State<AppState>,
    caller: Option<Extension<CallerClass>>,
    headers: HeaderMap,
) -> Response {
    if let Some(refused) = refuse_off_link(caller, &headers, "clear") {
        return refused;
    }
    let written = update_config(&state.pairing_paths.config, |root| {
        let [section, sub] = KEY_SECTIONS;
        let removed = root
            .get_mut(section)
            .and_then(|s| s.get_mut(sub))
            .and_then(Yaml::as_mapping_mut)
            .is_some_and(|api| api.remove(KEY_LEAF).is_some());
        Ok(removed)
    });
    match written {
        Ok(write) => {
            forget_config_snapshot(&state.pairing_paths.config);
            tracing::info!(was_set = write.value, "security.api_key_cleared");
            Json(json!({ "ok": true })).into_response()
        }
        Err(e) => write_failed(&e, "clear"),
    }
}

#[cfg(test)]
mod tests;
