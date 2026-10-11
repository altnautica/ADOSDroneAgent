//! The auth decision for PROXIED (non-native) routes.
//!
//! The native front already authenticates the routes it serves itself
//! ([`crate::serve::tcp_edge`]). The routes it does NOT serve fall through to
//! the reverse proxy. This module ports the two former Python auth middlewares
//! into Rust so the front runs the SAME decision on a proxied request before
//! forwarding it, making the front the single authenticator for the whole
//! surface (the residual Python no longer carries its own auth layers).
//!
//! Two auth gates are ported:
//!
//! 1. The API-key gate: the exempt set, the OPTIONS bypass, on-box trust, the
//!    cloud-posture setup routes, the setup-mutation routes, the unpaired-open
//!    pass, and the paired `X-ADOS-Key` requirement.
//! 2. The HMAC/replay gate: a request signature over the method, the request
//!    target, the nonce, the timestamp and the body, plus a nonce/timestamp
//!    replay window, applied to mutating methods when HMAC is enabled.
//!
//! Status codes, body shapes, and message strings are preserved byte-for-byte
//! from the predecessor middlewares so a GCS that surfaces the rejection body
//! reads stable text across the cutover.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::{Method, StatusCode};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

use ados_protocol::pairing_posture::{constant_time_eq, Pairing};

use crate::config::{ControlSecurityConfig, SecuritySection};
use crate::security_state::SecurityState;

mod predicates;
mod replay;
mod types;
pub(crate) use predicates::*;
pub use replay::*;
pub use types::*;

type HmacSha256 = Hmac<Sha256>;

/// The same-origin setup token lives at `/etc/ados/secrets/setup-token` — the
/// `SECRETS_DIR / "setup-token"` the Python `ados.core.paths.SETUP_TOKEN_PATH`
/// points at. Overridable via `ADOS_SETUP_TOKEN_PATH` for tests, mirroring the
/// env-override convention the sibling paths use.
pub const DEFAULT_SETUP_TOKEN_PATH: &str = "/etc/ados/secrets/setup-token";

/// Routes that never require authentication.
///
/// `/docs`, `/redoc` and `/openapi.json` are NOT here. They enumerated the
/// agent's entire route surface — including every path this file then tries
/// to gate — to any unauthenticated peer, which is a map of the attack
/// surface handed out for free. The residual FastAPI app now constructs with
/// `docs_url=None, redoc_url=None, openapi_url=None`, so they do not exist
/// to serve; keeping them exempt here would only have re-opened them the
/// moment someone re-enabled the app's defaults.
///
/// `/api/v1/setup/status` is NOT here: it reports LAN addresses, public tunnel
/// URLs, the backend and the MAVLink WebSocket URL, which is reconnaissance
/// against a paired aircraft. While unpaired the general posture below opens
/// it to the setup wizard; once paired it needs a credential like any route.
const EXEMPT_PATHS: &[&str] = &[
    "/",
    "/api/pairing/info",
    "/api/pairing/code",
    "/api/pairing/claim",
];

/// Cosmetic setup-wizard mutations under the same-origin trust model. Mirrors
/// `auth.py` `SAME_ORIGIN_SETUP_PATHS`.
const SAME_ORIGIN_SETUP_PATHS: &[&str] = &[
    "/api/v1/setup/finish",
    "/api/v1/setup/skip",
    "/api/v1/setup/reset",
];

/// Cloud-posture-mutating setup routes that always require a credential, even
/// while unpaired. Mirrors `auth.py` `SAME_ORIGIN_SETUP_CLOUD_PATHS`.
const SAME_ORIGIN_SETUP_CLOUD_PATHS: &[&str] = &[
    "/api/v1/setup/remote-access/cloudflare",
    "/api/v1/setup/cloud-choice",
];

/// Path prefixes that follow the same same-origin trust model as the exact
/// setup-mutation set. Mirrors `auth.py` `SAME_ORIGIN_SETUP_PREFIXES`.
const SAME_ORIGIN_SETUP_PREFIXES: &[&str] = &["/api/v1/setup/step/", "/api/v1/setup/nudges"];

/// Hostnames the agent itself binds; a request whose Origin/Referer host
/// matches one is same-origin. Mirrors `auth.py` `LOCAL_HOST_DEFAULTS`.
///
/// Parity gap: the Python set is augmented at runtime by the discovered
/// listener IPs (`setup/service.py`). That augmentation is NOT mirrored here —
/// a same-origin request whose host is a runtime-discovered LAN IP that is
/// neither one of these defaults nor the request's own `Host` header would be
/// accepted by the residual Python's middleware but not by this gate. In
/// practice the `host == request_host` fallback below covers the common case (a
/// browser served the webapp from `http://<lan-ip>:8080` sends Origin host ==
/// Host host), so this gap only bites a cross-host same-origin setup mutation,
/// which is not a real flow.
const LOCAL_HOST_DEFAULTS: &[&str] = &["localhost", "127.0.0.1", "192.168.4.1", "192.168.7.1"];

/// The one mutation exempt from HMAC verification even when HMAC is enabled:
/// the pairing claim, made by a fresh operator who holds no secret yet. Every
/// other pairing mutation (unpair, accept) is signed like any write.
const HMAC_EXEMPT_ROUTES: &[&str] = &["/api/pairing/claim"];

/// The replay window in seconds. Mirrors the `window_seconds=300.0` the
/// `SecurityMiddleware` constructs its `ReplayDetector` with.
const REPLAY_WINDOW_SECONDS: f64 = 300.0;

/// The nonce-store cap. Mirrors `max_nonces=50000` in the middleware.
const REPLAY_MAX_NONCES: usize = 50_000;

/// The minimum HMAC secret length (in bytes) the gate activates at. Mirrors the
/// `len(secret_bytes) >= 16` guard in `SecurityMiddleware.__init__` and the
/// `HmacSigner` constructor.
const HMAC_MIN_SECRET_LEN: usize = 16;

/// The header an upload carries its body digest on: the lowercase hex
/// SHA-256 of the body. On the streaming upload routes the HMAC covers this
/// digest instead of the body, so a large body is never buffered to verify.
pub const CONTENT_DIGEST_HEADER: &str = "x-content-sha256";

/// The routes whose bodies (plugin archives, vision models) are too large to
/// buffer for the signature. Each route hashes the body while it streams and
/// rejects a mismatch with the declared digest.
const DIGEST_SIGNED_ROUTES: &[&str] = &[
    "/api/plugins/parse",
    "/api/plugins/install",
    "/api/vision/models/upload",
];

/// The auth decision for a PROXIED route, holding the live security posture +
/// the shared replay detector + the setup-token path. Built once at startup and
/// shared behind an `Arc` in the edge state.
pub struct ProxiedAuth {
    security: Arc<SecurityState>,
    replay: ReplayDetector,
    setup_token_path: PathBuf,
}

/// The default setup-token path, honouring `ADOS_SETUP_TOKEN_PATH`.
fn default_setup_token_path() -> PathBuf {
    std::env::var("ADOS_SETUP_TOKEN_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_SETUP_TOKEN_PATH))
}

impl ProxiedAuth {
    /// Build over the live security posture, the standard setup-token path (or
    /// its `ADOS_SETUP_TOKEN_PATH` override), and a fresh 300s/50000 replay
    /// detector. Every decision reads the posture per request.
    pub fn live(security: Arc<SecurityState>) -> Self {
        Self {
            security,
            replay: ReplayDetector::default_security(),
            setup_token_path: default_setup_token_path(),
        }
    }

    /// Build over a fixed `security:` slice that never changes (tests, tools).
    pub fn new(config: SecuritySection) -> Self {
        Self::live(fixed_security(config))
    }

    /// Build over a fixed slice with an explicit setup-token path + detector
    /// (tests).
    pub fn with_paths(
        config: SecuritySection,
        setup_token_path: PathBuf,
        replay: ReplayDetector,
    ) -> Self {
        Self {
            security: fixed_security(config),
            replay,
            setup_token_path,
        }
    }

    /// The `security:` slice right now (the defaults while unreadable).
    fn config(&self) -> Arc<ControlSecurityConfig> {
        self.security.config_or_default()
    }

    /// Whether the HMAC gate is active: enabled in config AND the secret is at
    /// least the minimum length. Mirrors the predecessor middleware's
    /// `enabled and bool(secret)` plus the `>= 16` length guard.
    pub fn hmac_active(&self) -> bool {
        let cfg = self.config();
        cfg.security.hmac_enabled && cfg.security.hmac_secret.len() >= HMAC_MIN_SECRET_LEN
    }

    /// True when the HMAC gate would read the request body for THIS request: the
    /// gate is active, the method mutates, and the path is not HMAC-exempt. The
    /// caller buffers the body only in this case (otherwise it streams).
    pub fn hmac_needs_body(&self, method: &Method, path: &str) -> bool {
        self.hmac_active() && is_verified_method(method) && !is_hmac_exempt(path)
    }

    /// True when `path` is a streaming upload whose signature may cover the
    /// declared body digest (`X-Content-SHA256`) instead of the body.
    pub fn is_digest_signed_route(method: &Method, path: &str) -> bool {
        *method == Method::POST && contains(DIGEST_SIGNED_ROUTES, path)
    }

    /// The API-key gate (the `ApiKeyAuthMiddleware.dispatch` order). Returns
    /// `Accept` to pass to the HMAC gate / forward, or `Reject` with the exact
    /// Python status + body. `on_box` is the front's resolved on-box trust for
    /// this peer (the front computes it once in `tcp_edge`).
    pub fn decide_api_key(
        &self,
        method: &Method,
        path: &str,
        headers: &RequestHeaders,
        on_box: bool,
        pairing: &Pairing,
    ) -> Decision {
        // 1. Exempt routes (the static-asset and pairing-handshake set).
        if is_exempt(path) {
            return Decision::Accept;
        }
        // 2. OPTIONS (CORS preflight).
        if method == Method::OPTIONS {
            return Decision::Accept;
        }
        // 3. On-box loopback trust.
        if on_box {
            return Decision::Accept;
        }
        // 4. Cloud-posture-mutating setup routes: require a valid key OR a valid
        //    setup token, BEFORE the unpaired-open pass, so a fresh agent cannot
        //    have its cloud posture flipped by a forged-Origin LAN caller.
        if contains(SAME_ORIGIN_SETUP_CLOUD_PATHS, path) {
            if self.valid_api_key(headers, pairing) || self.valid_setup_token(headers) {
                return Decision::Accept;
            }
            return reject(
                StatusCode::UNAUTHORIZED,
                BodyField::Detail,
                messages::CLOUD_POSTURE,
            );
        }
        // 5. Cosmetic setup-wizard mutations under the same-origin trust model,
        //    with the token escalation knob. The same-origin shortcut holds only
        //    on an unpaired node: Origin and Host are caller-chosen, so once
        //    paired a wizard mutation needs the key like every other route.
        let is_setup_mutation = contains(SAME_ORIGIN_SETUP_PATHS, path)
            || SAME_ORIGIN_SETUP_PREFIXES
                .iter()
                .any(|p| path.starts_with(p));
        if is_setup_mutation {
            let require_token = self.config().security.setup_token_required;
            if !require_token && matches!(pairing, Pairing::Unpaired) && is_same_origin(headers) {
                return Decision::Accept;
            }
            if require_token {
                if self.valid_setup_token(headers) {
                    return Decision::Accept;
                }
                return reject(
                    StatusCode::UNAUTHORIZED,
                    BodyField::Detail,
                    messages::SETUP_TOKEN,
                );
            }
            // require_token false but not same-origin: fall through to the
            // general paired/unpaired posture below (matches the Python, which
            // does not return from the setup-mutation block in this case).
        }
        // 6. Unpaired ⇒ all routes open.
        if matches!(pairing, Pairing::Unpaired) {
            return Decision::Accept;
        }
        // 7. Paired ⇒ require X-ADOS-Key; missing vs invalid get distinct 401s.
        match headers.x_ados_key.as_deref() {
            None | Some("") => reject(
                StatusCode::UNAUTHORIZED,
                BodyField::Detail,
                messages::MISSING_KEY,
            ),
            Some(_) => {
                if self.valid_api_key(headers, pairing) {
                    Decision::Accept
                } else {
                    reject(
                        StatusCode::UNAUTHORIZED,
                        BodyField::Detail,
                        messages::INVALID_KEY,
                    )
                }
            }
        }
    }

    /// The HMAC/replay gate, run after the API-key gate accepts. `path` is the
    /// normalized decision path and `query` the raw query string, together the
    /// request target the signature covers. `body` is the buffered request body
    /// the caller read (only read when [`Self::hmac_needs_body`] is true).
    /// Returns `Accept` to forward or a `Reject` with the status + `{"error"}`
    /// body.
    pub fn decide_hmac(
        &self,
        method: &Method,
        path: &str,
        query: Option<&str>,
        headers: &RequestHeaders,
        body: &[u8],
    ) -> Decision {
        self.decide_signed(method, path, query, headers, body)
    }

    /// The HMAC/replay gate for a streaming upload that declared its body
    /// digest. The signature covers `METHOD \n path[?query] \n nonce \n
    /// be_f64(ts) \n <digest>`, so the body itself is never buffered; the route
    /// hashes the body as it streams and rejects a mismatch with `digest`.
    pub fn decide_hmac_digest(
        &self,
        method: &Method,
        path: &str,
        query: Option<&str>,
        headers: &RequestHeaders,
        digest: &str,
    ) -> Decision {
        let signed_payload = format!("\n{digest}");
        self.decide_signed(method, path, query, headers, signed_payload.as_bytes())
    }

    /// The shared gate: `body` is what the signature covers after the
    /// timestamp (the request body, or a newline and the body digest).
    fn decide_signed(
        &self,
        method: &Method,
        path: &str,
        query: Option<&str>,
        headers: &RequestHeaders,
        body: &[u8],
    ) -> Decision {
        let cfg = self.config();
        // Pass through if the gate is not active.
        if !(cfg.security.hmac_enabled && cfg.security.hmac_secret.len() >= HMAC_MIN_SECRET_LEN) {
            return Decision::Accept;
        }
        // Skip non-mutating methods.
        if !is_verified_method(method) {
            return Decision::Accept;
        }
        // Skip exempt routes + the whole pairing surface.
        if is_hmac_exempt(path) {
            return Decision::Accept;
        }
        // Extract the security headers; any missing → 401.
        let (Some(ts_str), Some(nonce), Some(sig)) = (
            non_empty(headers.x_timestamp.as_deref()),
            non_empty(headers.x_nonce.as_deref()),
            non_empty(headers.x_hmac_signature.as_deref()),
        ) else {
            return reject(
                StatusCode::UNAUTHORIZED,
                BodyField::Error,
                messages::MISSING_SECURITY_HEADERS,
            );
        };
        // Parse the timestamp as f64; a bad or non-finite value → 401. A NaN
        // timestamp would compare false against the freshness window and pass it.
        let timestamp = match ts_str.parse::<f64>() {
            Ok(t) if t.is_finite() => t,
            _ => {
                return reject(
                    StatusCode::UNAUTHORIZED,
                    BodyField::Error,
                    messages::BAD_TIMESTAMP,
                )
            }
        };
        // Verify the signature over the whole request identity → 401 on mismatch.
        // Checked BEFORE the replay store records the nonce, so a caller without
        // the secret can neither grow the store nor burn a nonce a signed request
        // will later present.
        let signed = SignedRequest {
            method: method.as_str(),
            path,
            query,
            nonce,
            timestamp,
            body,
        };
        if !verify_hmac(cfg.security.hmac_secret.as_bytes(), &signed, sig) {
            return reject(
                StatusCode::UNAUTHORIZED,
                BodyField::Error,
                messages::INVALID_SIGNATURE,
            );
        }
        // Replay / freshness (window + nonce de-dup) → 403.
        if !self.replay.check(timestamp, nonce) {
            return reject(StatusCode::FORBIDDEN, BodyField::Error, messages::REPLAY);
        }
        Decision::Accept
    }

    /// True when the request carries a valid API key: the manually-configured
    /// `security.api.api_key` (constant-time compared) OR a key matching the
    /// pairing posture. Mirrors `auth.py` `_valid_api_key`. A missing/empty key
    /// is not valid.
    fn valid_api_key(&self, headers: &RequestHeaders, pairing: &Pairing) -> bool {
        crate::auth::credential_matches(
            pairing,
            &self.config().security.api.api_key,
            headers.x_ados_key.as_deref(),
        )
    }

    /// Whether `path` is a cloud-posture route that a dashboard session must
    /// not unlock: it demands a real key or the setup token.
    pub fn is_cloud_posture_path(path: &str) -> bool {
        contains(SAME_ORIGIN_SETUP_CLOUD_PATHS, path)
    }

    /// True when the request carries the valid `X-ADOS-Setup-Token`. Mirrors
    /// `auth.py` `_valid_setup_token` + `_load_setup_token`: read the on-disk
    /// token, compare to the header. A missing header or absent token file → no.
    fn valid_setup_token(&self, headers: &RequestHeaders) -> bool {
        let Some(provided) = non_empty(headers.x_ados_setup_token.as_deref()) else {
            return false;
        };
        let Some(expected) = self.load_setup_token() else {
            return false;
        };
        // The Python compares with plain `==`; use a constant-time compare here
        // (strictly stronger, same accept/reject result).
        constant_time_eq(provided.as_bytes(), expected.as_bytes())
    }

    /// Read + trim the setup token from disk, returning `None` on an absent /
    /// unreadable / empty file.
    fn load_setup_token(&self) -> Option<String> {
        let text = std::fs::read_to_string(&self.setup_token_path).ok()?;
        let trimmed = text.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }
}

/// A posture that never changes, wrapping a bare `security:` slice.
fn fixed_security(config: SecuritySection) -> Arc<SecurityState> {
    Arc::new(SecurityState::fixed(ControlSecurityConfig {
        security: config,
        ..Default::default()
    }))
}

#[cfg(test)]
mod tests;
