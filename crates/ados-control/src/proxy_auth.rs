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
//! 2. The HMAC/replay gate: a request signature over the timestamp + body, plus
//!    a nonce/timestamp replay window, applied to mutating methods when HMAC is
//!    enabled.
//!
//! Status codes, body shapes, and message strings are preserved byte-for-byte
//! from the predecessor middlewares so a GCS that surfaces the rejection body
//! reads stable text across the cutover.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::{Method, StatusCode};
use hmac::{Hmac, Mac};
use sha2::Sha256;

use ados_protocol::pairing_posture::{constant_time_eq, data_plane_access, Access, Pairing};

use crate::config::SecuritySection;

type HmacSha256 = Hmac<Sha256>;

/// The same-origin setup token lives at `/etc/ados/secrets/setup-token` — the
/// `SECRETS_DIR / "setup-token"` the Python `ados.core.paths.SETUP_TOKEN_PATH`
/// points at. Overridable via `ADOS_SETUP_TOKEN_PATH` for tests, mirroring the
/// env-override convention the sibling paths use.
pub const DEFAULT_SETUP_TOKEN_PATH: &str = "/etc/ados/secrets/setup-token";

/// Routes that never require authentication. Mirrors the former `EXEMPT_PATHS`.
const EXEMPT_PATHS: &[&str] = &[
    "/",
    "/docs",
    "/openapi.json",
    "/redoc",
    "/api/pairing/info",
    "/api/pairing/code",
    "/api/pairing/claim",
    "/api/v1/setup/status",
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

/// Routes exempt from HMAC verification even when HMAC is enabled. Mirrors
/// `security.py` `EXEMPT_ROUTES`.
const HMAC_EXEMPT_ROUTES: &[&str] = &[
    "/",
    "/docs",
    "/openapi.json",
    "/api/pairing/claim",
    "/api/pairing/status",
];

/// The replay window in seconds. Mirrors the `window_seconds=300.0` the
/// `SecurityMiddleware` constructs its `ReplayDetector` with.
const REPLAY_WINDOW_SECONDS: f64 = 300.0;

/// The nonce-store cap. Mirrors `max_nonces=50000` in the middleware.
const REPLAY_MAX_NONCES: usize = 50_000;

/// The minimum HMAC secret length (in bytes) the gate activates at. Mirrors the
/// `len(secret_bytes) >= 16` guard in `SecurityMiddleware.__init__` and the
/// `HmacSigner` constructor.
const HMAC_MIN_SECRET_LEN: usize = 16;

/// The headers the decision reads off a request, captured before the body is
/// touched so the decision is a pure function of typed inputs (the caller pulls
/// them out of the live `HeaderMap` once).
#[derive(Debug, Clone, Default)]
pub struct RequestHeaders {
    pub origin: Option<String>,
    pub referer: Option<String>,
    pub host: Option<String>,
    pub x_ados_key: Option<String>,
    pub x_ados_setup_token: Option<String>,
    /// The dashboard-access session token (`X-ADOS-Dashboard-Session`). Not read
    /// by [`ProxiedAuth::decide_api_key`] itself — the serve edge consults it to
    /// reverse a rejection, so the session is an alternative credential on proxied
    /// routes too.
    pub x_ados_dashboard_session: Option<String>,
    pub x_timestamp: Option<String>,
    pub x_nonce: Option<String>,
    pub x_hmac_signature: Option<String>,
}

/// The outcome of the proxied-route auth decision. `Accept` forwards the
/// request to the residual upstream; `Reject` is turned into a response by the
/// caller with the exact status + `{"detail"|"error": ...}` body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Forward the request to the proxy upstream.
    Accept,
    /// Stop and answer with this status and JSON body.
    Reject {
        status: StatusCode,
        /// The error message text. `field` selects whether it renders under the
        /// `detail` key (the API-key middleware) or `error` (the HMAC
        /// middleware), matching each Python middleware's body shape.
        field: BodyField,
        message: &'static str,
    },
}

/// Which JSON key the rejection body renders the message under, mirroring the
/// two Python middlewares: the API-key middleware uses `{"detail": ...}`, the
/// HMAC middleware uses `{"error": ...}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyField {
    Detail,
    Error,
}

/// The fixed message strings, kept verbatim from the Python so a GCS reading the
/// body sees identical text against either surface.
mod messages {
    pub const CLOUD_POSTURE: &str = "This setup route changes cloud posture and \
requires the API key (X-ADOS-Key) or the setup token (X-ADOS-Setup-Token, \
printed by `ados status`).";
    pub const SETUP_TOKEN: &str = "Missing or invalid X-ADOS-Setup-Token header. \
Setup token is printed by the local CLI.";
    pub const MISSING_KEY: &str = "Missing X-ADOS-Key header. This agent is paired \
and requires authentication. Run `ados status` on the agent to print the API \
key, then enter it in the dashboard.";
    pub const INVALID_KEY: &str = "Invalid API key";
    pub const MISSING_SECURITY_HEADERS: &str =
        "Missing security headers (X-Timestamp, X-Nonce, X-HMAC-Signature)";
    pub const BAD_TIMESTAMP: &str = "Invalid X-Timestamp format";
    pub const REPLAY: &str = "Request rejected (replay detected or timestamp expired)";
    pub const INVALID_SIGNATURE: &str = "Invalid HMAC signature";
}

/// The nonce/timestamp replay detector, mirroring `security/replay.py`. Holds a
/// `nonce -> timestamp` map behind a mutex (the front shares one detector across
/// all connections), pruning expired nonces on the same cadence as the Python:
/// every `window` seconds, or whenever the store exceeds `max_nonces`.
pub struct ReplayDetector {
    window: f64,
    max_nonces: usize,
    state: Mutex<ReplayState>,
}

struct ReplayState {
    nonces: HashMap<String, f64>,
    last_prune: f64,
}

impl ReplayDetector {
    /// A detector with the given window (seconds) and nonce cap.
    pub fn new(window_seconds: f64, max_nonces: usize) -> Self {
        Self {
            window: window_seconds,
            max_nonces,
            state: Mutex::new(ReplayState {
                nonces: HashMap::new(),
                last_prune: 0.0,
            }),
        }
    }

    /// The 300s / 50000-nonce detector the `SecurityMiddleware` builds.
    pub fn default_security() -> Self {
        Self::new(REPLAY_WINDOW_SECONDS, REPLAY_MAX_NONCES)
    }

    /// True when the message is fresh (timestamp within the window AND the nonce
    /// is unseen), false when it should be rejected. Mirrors
    /// `ReplayDetector.check`: an out-of-window timestamp or a duplicate nonce
    /// is rejected; a fresh message records its nonce and prunes when due.
    /// `now` is supplied so a test can pin the clock.
    pub fn check_at(&self, timestamp: f64, nonce: &str, now: f64) -> bool {
        // Reject messages outside the time window.
        let age = (now - timestamp).abs();
        if age > self.window {
            return false;
        }
        let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        // Reject duplicate nonces.
        if s.nonces.contains_key(nonce) {
            return false;
        }
        // Store the nonce.
        s.nonces.insert(nonce.to_string(), timestamp);
        // Prune if due (every window, or when the store is oversized).
        if now - s.last_prune > self.window || s.nonces.len() > self.max_nonces {
            let cutoff = now - self.window;
            s.nonces.retain(|_, ts| *ts >= cutoff);
            s.last_prune = now;
        }
        true
    }

    /// `check_at` against the real clock.
    pub fn check(&self, timestamp: f64, nonce: &str) -> bool {
        self.check_at(timestamp, nonce, unix_now())
    }
}

/// Wall-clock seconds since the epoch as `f64`, matching Python `time.time()`.
fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// The auth decision for a PROXIED route, holding the config slice + the shared
/// replay detector + the setup-token path. Built once at startup and shared
/// behind an `Arc` in the edge state.
pub struct ProxiedAuth {
    config: SecuritySection,
    replay: ReplayDetector,
    setup_token_path: PathBuf,
}

impl ProxiedAuth {
    /// Build from the loaded `security:` config slice, the standard setup-token
    /// path (or its `ADOS_SETUP_TOKEN_PATH` override), and a fresh 300s/50000
    /// replay detector.
    pub fn new(config: SecuritySection) -> Self {
        let setup_token_path = std::env::var("ADOS_SETUP_TOKEN_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_SETUP_TOKEN_PATH));
        Self {
            config,
            replay: ReplayDetector::default_security(),
            setup_token_path,
        }
    }

    /// Build with an explicit setup-token path + detector (tests).
    pub fn with_paths(
        config: SecuritySection,
        setup_token_path: PathBuf,
        replay: ReplayDetector,
    ) -> Self {
        Self {
            config,
            replay,
            setup_token_path,
        }
    }

    /// Whether the HMAC gate is active: enabled in config AND the secret is at
    /// least the minimum length. Mirrors the predecessor middleware's
    /// `enabled and bool(secret)` plus the `>= 16` length guard.
    pub fn hmac_active(&self) -> bool {
        self.config.hmac_enabled && self.config.hmac_secret.len() >= HMAC_MIN_SECRET_LEN
    }

    /// True when the HMAC gate would read the request body for THIS request: the
    /// gate is active, the method mutates, and the path is not HMAC-exempt. The
    /// caller buffers the body only in this case (otherwise it streams).
    pub fn hmac_needs_body(&self, method: &Method, path: &str) -> bool {
        self.hmac_active() && is_verified_method(method) && !is_hmac_exempt(path)
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
        //    with the token escalation knob.
        let is_setup_mutation = contains(SAME_ORIGIN_SETUP_PATHS, path)
            || SAME_ORIGIN_SETUP_PREFIXES
                .iter()
                .any(|p| path.starts_with(p));
        if is_setup_mutation {
            let require_token = self.config.setup_token_required;
            if !require_token && is_same_origin(headers) {
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

    /// The HMAC/replay gate (`SecurityMiddleware.dispatch`), run after the
    /// API-key gate accepts. `body` is the buffered request body the caller read
    /// (only read when [`hmac_needs_body`] is true). Returns `Accept` to forward
    /// or a `Reject` with the exact Python status + `{"error"}` body.
    pub fn decide_hmac(
        &self,
        method: &Method,
        path: &str,
        headers: &RequestHeaders,
        body: &[u8],
    ) -> Decision {
        // Pass through if the gate is not active.
        if !self.hmac_active() {
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
        // Parse the timestamp as f64; a bad value → 401.
        let Ok(timestamp) = ts_str.parse::<f64>() else {
            return reject(
                StatusCode::UNAUTHORIZED,
                BodyField::Error,
                messages::BAD_TIMESTAMP,
            );
        };
        // A non-finite timestamp (NaN/inf) is not a usable value either — Python
        // `float("nan")` parses but then `abs(now - nan)` is NaN and `NaN > w` is
        // False, so the Python would NOT reject it at the replay step. Mirror
        // that: only reject here on a parse failure, leave finiteness to the
        // replay window check (NaN age compares false, so it would pass the
        // window; the nonce store still de-dups). Keep behavior identical by not
        // adding a finiteness guard the Python lacks.
        // Replay / freshness (window + nonce de-dup) → 403.
        if !self.replay.check(timestamp, nonce) {
            return reject(StatusCode::FORBIDDEN, BodyField::Error, messages::REPLAY);
        }
        // Verify the signature over (be_f64(ts) || body) → 401 on mismatch.
        if !verify_hmac(self.config.hmac_secret.as_bytes(), timestamp, body, sig) {
            return reject(
                StatusCode::UNAUTHORIZED,
                BodyField::Error,
                messages::INVALID_SIGNATURE,
            );
        }
        Decision::Accept
    }

    /// True when the request carries a valid API key: the manually-configured
    /// `security.api.api_key` (constant-time compared) OR a key matching the
    /// pairing posture. Mirrors `auth.py` `_valid_api_key`. A missing/empty key
    /// is not valid.
    fn valid_api_key(&self, headers: &RequestHeaders, pairing: &Pairing) -> bool {
        let Some(key) = non_empty(headers.x_ados_key.as_deref()) else {
            return false;
        };
        let configured = self.config.api.api_key.as_str();
        if !configured.is_empty() && constant_time_eq(key.as_bytes(), configured.as_bytes()) {
            return true;
        }
        // The pairing key compare: reuse the shared posture (on_box=false here —
        // this is only consulted off-box). Accept only when the key matches.
        data_plane_access(pairing, false, Some(key)) == Access::Accept
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
    /// unreadable / empty file. Mirrors `_load_setup_token`.
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

/// `path in EXEMPT_PATHS or path.startswith("/docs") or not path.startswith("/api/")`.
/// Mirrors `auth.py` `is_exempt`: the exempt set, `/docs*`, and every non-`/api/`
/// path (the static SPA assets served from `/`).
fn is_exempt(path: &str) -> bool {
    if contains(EXEMPT_PATHS, path) || path.starts_with("/docs") {
        return true;
    }
    !path.starts_with("/api/")
}

/// True when the path is exempt from HMAC verification: the HMAC-exempt set OR
/// any `/api/pairing/` path. Mirrors `security.py`'s exempt-route check + the
/// `path.startswith("/api/pairing/")` skip.
fn is_hmac_exempt(path: &str) -> bool {
    contains(HMAC_EXEMPT_ROUTES, path) || path.starts_with("/api/pairing/")
}

/// The mutating methods the HMAC gate verifies. Mirrors `VERIFIED_METHODS`.
fn is_verified_method(method: &Method) -> bool {
    matches!(
        *method,
        Method::POST | Method::PUT | Method::DELETE | Method::PATCH
    )
}

/// True when the request's Origin/Referer host points at this agent. Mirrors
/// `auth.py` `_is_same_origin` + `_origin_host`: the host comes from Origin or
/// (falling back to) Referer; it is same-origin when it is one of the local
/// defaults OR equals the request's own `Host` header host.
fn is_same_origin(headers: &RequestHeaders) -> bool {
    let Some(host) = origin_host(headers) else {
        // No Origin / Referer → a server-to-server caller, not same-origin.
        return false;
    };
    if LOCAL_HOST_DEFAULTS.contains(&host.as_str()) {
        return true;
    }
    let request_host = headers
        .host
        .as_deref()
        .map(|h| h.split(':').next().unwrap_or(h).to_string())
        .unwrap_or_default();
    !request_host.is_empty() && host == request_host
}

/// The host part of the Origin (or Referer) header. Mirrors `_origin_host`
/// (`urlparse(origin).hostname`): the host between `scheme://` and the next
/// `/`, `:`, `?`, or `#`. Returns `None` when neither header is present or the
/// value has no host.
fn origin_host(headers: &RequestHeaders) -> Option<String> {
    let raw = headers.origin.as_deref().or(headers.referer.as_deref())?;
    parse_hostname(raw)
}

/// Extract the hostname from a URL the way `urllib.parse.urlparse(...).hostname`
/// does: drop the scheme, drop any `userinfo@`, take up to the next `/?#`, drop
/// a `:port` suffix, lowercase. A bracketed IPv6 literal keeps its inner host.
/// Returns `None` when there is no host component.
fn parse_hostname(url: &str) -> Option<String> {
    // Strip the scheme (`scheme://`); a value with no `://` has no netloc, so it
    // has no hostname (matches urlparse, which puts a bare `host/path` in the
    // path, not the netloc).
    let after_scheme = url.split_once("://").map(|(_, rest)| rest)?;
    // The netloc ends at the first `/`, `?`, or `#`.
    let netloc_end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    let netloc = &after_scheme[..netloc_end];
    // Drop any `userinfo@`.
    let host_port = netloc.rsplit_once('@').map(|(_, h)| h).unwrap_or(netloc);
    // A bracketed IPv6 literal: take the inside of the brackets.
    let host = if let Some(rest) = host_port.strip_prefix('[') {
        rest.split_once(']').map(|(inner, _)| inner).unwrap_or(rest)
    } else {
        // Drop a `:port` suffix.
        host_port.split(':').next().unwrap_or(host_port)
    };
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

/// Verify a hex HMAC signature over `be_f64(timestamp) || body`. Mirrors
/// `HmacSigner.sign`/`verify`: the message is the big-endian IEEE-754 double of
/// the timestamp followed by the raw body, the signature is the lowercase hex of
/// HMAC-SHA256, compared constant-time.
fn verify_hmac(secret: &[u8], timestamp: f64, body: &[u8], presented_hex: &str) -> bool {
    let mut mac = match HmacSha256::new_from_slice(secret) {
        Ok(m) => m,
        // The caller only reaches here when the secret is >= 16 bytes, so a key
        // error cannot happen; reject defensively if it ever did.
        Err(_) => return false,
    };
    mac.update(&timestamp.to_be_bytes());
    mac.update(body);
    let expected = mac.finalize().into_bytes();
    let expected_hex = hex::encode(expected);
    // Compare the hex strings constant-time, mirroring Python's
    // `hmac.compare_digest(expected_hex, signature)`.
    constant_time_eq(expected_hex.as_bytes(), presented_hex.as_bytes())
}

/// `value in set`, for the `&[&str]` constant sets.
fn contains(set: &[&str], value: &str) -> bool {
    set.contains(&value)
}

/// `Some(s)` when the option holds a non-empty string, else `None`. Mirrors the
/// Python truthiness check on a header (`if not api_key` etc.): a present-but-
/// empty header is treated as absent.
fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|s| !s.is_empty())
}

/// Build a `Reject` decision.
fn reject(status: StatusCode, field: BodyField, message: &'static str) -> Decision {
    Decision::Reject {
        status,
        field,
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paired(key: &str) -> Pairing {
        Pairing::Paired(key.to_string())
    }

    fn cfg() -> SecuritySection {
        SecuritySection::default()
    }

    /// An auth with no configured key, HMAC off, no token file.
    fn auth_basic() -> ProxiedAuth {
        ProxiedAuth::with_paths(
            cfg(),
            PathBuf::from("/nonexistent/setup-token"),
            ReplayDetector::default_security(),
        )
    }

    fn headers_with_key(key: &str) -> RequestHeaders {
        RequestHeaders {
            x_ados_key: Some(key.to_string()),
            ..Default::default()
        }
    }

    // ---- exempt + static-asset ----

    #[test]
    fn exempt_paths_accept_even_when_paired() {
        let auth = auth_basic();
        let p = paired("k");
        for path in EXEMPT_PATHS {
            assert_eq!(
                auth.decide_api_key(&Method::GET, path, &RequestHeaders::default(), false, &p),
                Decision::Accept,
                "{path} should be exempt"
            );
        }
    }

    #[test]
    fn docs_prefix_and_static_assets_accept() {
        let auth = auth_basic();
        let p = paired("k");
        // /docs* prefix.
        assert_eq!(
            auth.decide_api_key(
                &Method::GET,
                "/docs/oauth2-redirect",
                &RequestHeaders::default(),
                false,
                &p
            ),
            Decision::Accept
        );
        // Non-/api/ static SPA asset.
        assert_eq!(
            auth.decide_api_key(
                &Method::GET,
                "/assets/app.js",
                &RequestHeaders::default(),
                false,
                &p
            ),
            Decision::Accept
        );
        // An /api/ route is NOT a static asset → gated when paired with no key.
        assert!(matches!(
            auth.decide_api_key(
                &Method::GET,
                "/api/status",
                &RequestHeaders::default(),
                false,
                &p
            ),
            Decision::Reject { .. }
        ));
    }

    // ---- OPTIONS + on-box ----

    #[test]
    fn options_preflight_accepts() {
        let auth = auth_basic();
        let p = paired("k");
        assert_eq!(
            auth.decide_api_key(
                &Method::OPTIONS,
                "/api/status",
                &RequestHeaders::default(),
                false,
                &p
            ),
            Decision::Accept
        );
    }

    #[test]
    fn on_box_accepts_a_paired_route_with_no_key() {
        let auth = auth_basic();
        let p = paired("k");
        assert_eq!(
            auth.decide_api_key(
                &Method::POST,
                "/api/command",
                &RequestHeaders::default(),
                true,
                &p
            ),
            Decision::Accept
        );
    }

    // ---- cloud-posture setup routes ----

    #[test]
    fn cloud_posture_paths_reject_without_credential_even_unpaired() {
        let auth = auth_basic();
        for path in SAME_ORIGIN_SETUP_CLOUD_PATHS {
            let d = auth.decide_api_key(
                &Method::POST,
                path,
                &RequestHeaders::default(),
                false,
                &Pairing::Unpaired,
            );
            assert_eq!(
                d,
                reject(
                    StatusCode::UNAUTHORIZED,
                    BodyField::Detail,
                    messages::CLOUD_POSTURE
                ),
                "{path} must require a credential even while unpaired"
            );
        }
    }

    #[test]
    fn cloud_posture_path_accepts_with_a_valid_key() {
        // Paired agent, the request carries the matching key.
        let auth = auth_basic();
        let p = paired("the-key");
        assert_eq!(
            auth.decide_api_key(
                &Method::POST,
                "/api/v1/setup/cloud-choice",
                &headers_with_key("the-key"),
                false,
                &p,
            ),
            Decision::Accept
        );
    }

    #[test]
    fn cloud_posture_path_accepts_with_a_valid_setup_token() {
        let dir = tempfile::tempdir().unwrap();
        let token_path = dir.path().join("setup-token");
        std::fs::write(&token_path, "  tok-123\n").unwrap();
        let auth = ProxiedAuth::with_paths(cfg(), token_path, ReplayDetector::default_security());
        let headers = RequestHeaders {
            x_ados_setup_token: Some("tok-123".to_string()),
            ..Default::default()
        };
        assert_eq!(
            auth.decide_api_key(
                &Method::POST,
                "/api/v1/setup/remote-access/cloudflare",
                &headers,
                false,
                &Pairing::Unpaired,
            ),
            Decision::Accept
        );
    }

    // ---- setup mutations ----

    #[test]
    fn setup_mutation_same_origin_accepts_when_token_not_required() {
        let auth = auth_basic(); // setup_token_required defaults false
        let headers = RequestHeaders {
            origin: Some("http://192.168.4.1:8080".to_string()),
            host: Some("192.168.4.1:8080".to_string()),
            ..Default::default()
        };
        assert_eq!(
            auth.decide_api_key(
                &Method::POST,
                "/api/v1/setup/finish",
                &headers,
                false,
                &Pairing::Unpaired
            ),
            Decision::Accept
        );
        // A prefix route (step/) too.
        assert_eq!(
            auth.decide_api_key(
                &Method::POST,
                "/api/v1/setup/step/profile",
                &headers,
                false,
                &Pairing::Unpaired
            ),
            Decision::Accept
        );
    }

    #[test]
    fn setup_mutation_rejects_when_token_required_and_absent() {
        let mut c = cfg();
        c.setup_token_required = true;
        let auth = ProxiedAuth::with_paths(
            c,
            PathBuf::from("/nonexistent"),
            ReplayDetector::default_security(),
        );
        let headers = RequestHeaders {
            origin: Some("http://192.168.4.1:8080".to_string()),
            host: Some("192.168.4.1:8080".to_string()),
            ..Default::default()
        };
        assert_eq!(
            auth.decide_api_key(
                &Method::POST,
                "/api/v1/setup/skip",
                &headers,
                false,
                &Pairing::Unpaired
            ),
            reject(
                StatusCode::UNAUTHORIZED,
                BodyField::Detail,
                messages::SETUP_TOKEN
            ),
        );
    }

    #[test]
    fn setup_mutation_accepts_when_token_required_and_valid() {
        let dir = tempfile::tempdir().unwrap();
        let token_path = dir.path().join("setup-token");
        std::fs::write(&token_path, "tok-xyz").unwrap();
        let mut c = cfg();
        c.setup_token_required = true;
        let auth = ProxiedAuth::with_paths(c, token_path, ReplayDetector::default_security());
        let headers = RequestHeaders {
            x_ados_setup_token: Some("tok-xyz".to_string()),
            ..Default::default()
        };
        assert_eq!(
            auth.decide_api_key(
                &Method::POST,
                "/api/v1/setup/reset",
                &headers,
                false,
                &Pairing::Unpaired
            ),
            Decision::Accept
        );
    }

    #[test]
    fn setup_mutation_not_same_origin_falls_through_to_paired_gate() {
        // token not required, NOT same-origin, paired → falls through to the
        // paired-key requirement (matches the Python, which does not early-return
        // from the setup block in this branch).
        let auth = auth_basic();
        let p = paired("k");
        let headers = RequestHeaders::default(); // no origin → not same-origin
        let d = auth.decide_api_key(&Method::POST, "/api/v1/setup/finish", &headers, false, &p);
        assert_eq!(
            d,
            reject(
                StatusCode::UNAUTHORIZED,
                BodyField::Detail,
                messages::MISSING_KEY
            )
        );
    }

    // ---- unpaired open ----

    #[test]
    fn unpaired_opens_general_routes() {
        let auth = auth_basic();
        assert_eq!(
            auth.decide_api_key(
                &Method::POST,
                "/api/command",
                &RequestHeaders::default(),
                false,
                &Pairing::Unpaired
            ),
            Decision::Accept
        );
    }

    // ---- paired key ----

    #[test]
    fn paired_missing_key_is_a_distinct_401() {
        let auth = auth_basic();
        let p = paired("k");
        assert_eq!(
            auth.decide_api_key(
                &Method::GET,
                "/api/status",
                &RequestHeaders::default(),
                false,
                &p
            ),
            reject(
                StatusCode::UNAUTHORIZED,
                BodyField::Detail,
                messages::MISSING_KEY
            )
        );
        // An empty key reads as missing.
        assert_eq!(
            auth.decide_api_key(
                &Method::GET,
                "/api/status",
                &headers_with_key(""),
                false,
                &p
            ),
            reject(
                StatusCode::UNAUTHORIZED,
                BodyField::Detail,
                messages::MISSING_KEY
            )
        );
    }

    #[test]
    fn paired_wrong_key_is_invalid_key() {
        let auth = auth_basic();
        let p = paired("right-key");
        assert_eq!(
            auth.decide_api_key(
                &Method::GET,
                "/api/status",
                &headers_with_key("wrong-key"),
                false,
                &p
            ),
            reject(
                StatusCode::UNAUTHORIZED,
                BodyField::Detail,
                messages::INVALID_KEY
            )
        );
    }

    #[test]
    fn paired_pairing_key_accepts() {
        let auth = auth_basic();
        let p = paired("right-key");
        assert_eq!(
            auth.decide_api_key(
                &Method::GET,
                "/api/status",
                &headers_with_key("right-key"),
                false,
                &p
            ),
            Decision::Accept
        );
    }

    #[test]
    fn paired_configured_key_accepts_independent_of_pairing_key() {
        // The manually-configured security.api.api_key passes even when it
        // differs from the pairing key.
        let mut c = cfg();
        c.api.api_key = "configured-key".to_string();
        let auth = ProxiedAuth::with_paths(
            c,
            PathBuf::from("/nonexistent"),
            ReplayDetector::default_security(),
        );
        let p = paired("a-different-pairing-key");
        assert_eq!(
            auth.decide_api_key(
                &Method::GET,
                "/api/status",
                &headers_with_key("configured-key"),
                false,
                &p
            ),
            Decision::Accept
        );
        // And the pairing key still works alongside it.
        assert_eq!(
            auth.decide_api_key(
                &Method::GET,
                "/api/status",
                &headers_with_key("a-different-pairing-key"),
                false,
                &p
            ),
            Decision::Accept
        );
    }

    // ---- same-origin host parsing ----

    #[test]
    fn origin_host_parses_like_urlparse() {
        assert_eq!(
            parse_hostname("http://localhost:8080"),
            Some("localhost".to_string())
        );
        assert_eq!(
            parse_hostname("https://192.168.4.1/path?x=1"),
            Some("192.168.4.1".to_string())
        );
        assert_eq!(
            parse_hostname("http://user:pw@host.local:9/p"),
            Some("host.local".to_string())
        );
        assert_eq!(
            parse_hostname("http://[::1]:8080/x"),
            Some("::1".to_string())
        );
        assert_eq!(parse_hostname("not-a-url"), None);
        assert_eq!(parse_hostname("http://"), None);
        assert_eq!(
            parse_hostname("HTTP://LocalHost"),
            Some("localhost".to_string())
        );
    }

    #[test]
    fn same_origin_falls_back_to_referer_and_host_match() {
        // Referer used when Origin is absent; host == request Host host.
        let h = RequestHeaders {
            referer: Some("http://my-drone.local:8080/setup".to_string()),
            host: Some("my-drone.local:8080".to_string()),
            ..Default::default()
        };
        assert!(is_same_origin(&h));
        // A cross-origin host that matches neither a default nor the Host → not
        // same-origin.
        let h2 = RequestHeaders {
            origin: Some("http://evil.example/".to_string()),
            host: Some("my-drone.local:8080".to_string()),
            ..Default::default()
        };
        assert!(!is_same_origin(&h2));
        // No Origin/Referer → not same-origin.
        assert!(!is_same_origin(&RequestHeaders::default()));
    }

    // ---- HMAC / replay ----

    /// A known secret + a hand-computed signature to lock the wire format.
    fn hmac_auth(secret: &str) -> ProxiedAuth {
        let mut c = cfg();
        c.hmac_enabled = true;
        c.hmac_secret = secret.to_string();
        ProxiedAuth::with_paths(
            c,
            PathBuf::from("/nonexistent"),
            ReplayDetector::default_security(),
        )
    }

    fn sign(secret: &str, timestamp: f64, body: &[u8]) -> String {
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(&timestamp.to_be_bytes());
        mac.update(body);
        hex::encode(mac.finalize().into_bytes())
    }

    #[test]
    fn hmac_inactive_when_disabled_or_short_secret() {
        // Disabled → not active.
        let mut c = cfg();
        c.hmac_enabled = false;
        c.hmac_secret = "a-long-enough-secret-key".to_string();
        let a = ProxiedAuth::with_paths(c, PathBuf::from("/x"), ReplayDetector::default_security());
        assert!(!a.hmac_active());
        // Enabled but a < 16-byte secret → not active (matches the Python guard).
        let mut c2 = cfg();
        c2.hmac_enabled = true;
        c2.hmac_secret = "short".to_string();
        let a2 =
            ProxiedAuth::with_paths(c2, PathBuf::from("/x"), ReplayDetector::default_security());
        assert!(!a2.hmac_active());
        // A disabled gate accepts every body unconditionally.
        assert_eq!(
            a.decide_hmac(
                &Method::POST,
                "/api/command",
                &RequestHeaders::default(),
                b"{}"
            ),
            Decision::Accept
        );
    }

    #[test]
    fn hmac_skips_non_mutating_and_exempt() {
        let auth = hmac_auth("a-long-enough-secret-key");
        assert!(auth.hmac_active());
        // GET is not verified.
        assert_eq!(
            auth.decide_hmac(&Method::GET, "/api/status", &RequestHeaders::default(), b""),
            Decision::Accept
        );
        // An HMAC-exempt route.
        assert_eq!(
            auth.decide_hmac(
                &Method::POST,
                "/api/pairing/claim",
                &RequestHeaders::default(),
                b"{}"
            ),
            Decision::Accept
        );
        // Any /api/pairing/ path is exempt.
        assert_eq!(
            auth.decide_hmac(
                &Method::POST,
                "/api/pairing/whatever",
                &RequestHeaders::default(),
                b"{}"
            ),
            Decision::Accept
        );
        // needs_body is false for an exempt/non-mutating path, true for a real one.
        assert!(!auth.hmac_needs_body(&Method::GET, "/api/command"));
        assert!(!auth.hmac_needs_body(&Method::POST, "/api/pairing/claim"));
        assert!(auth.hmac_needs_body(&Method::POST, "/api/command"));
    }

    #[test]
    fn hmac_missing_headers_is_401() {
        let auth = hmac_auth("a-long-enough-secret-key");
        assert_eq!(
            auth.decide_hmac(
                &Method::POST,
                "/api/command",
                &RequestHeaders::default(),
                b"{}"
            ),
            reject(
                StatusCode::UNAUTHORIZED,
                BodyField::Error,
                messages::MISSING_SECURITY_HEADERS
            ),
        );
    }

    #[test]
    fn hmac_bad_timestamp_is_401() {
        let auth = hmac_auth("a-long-enough-secret-key");
        let headers = RequestHeaders {
            x_timestamp: Some("not-a-number".to_string()),
            x_nonce: Some("n1".to_string()),
            x_hmac_signature: Some("deadbeef".to_string()),
            ..Default::default()
        };
        assert_eq!(
            auth.decide_hmac(&Method::POST, "/api/command", &headers, b"{}"),
            reject(
                StatusCode::UNAUTHORIZED,
                BodyField::Error,
                messages::BAD_TIMESTAMP
            ),
        );
    }

    #[test]
    fn hmac_expired_timestamp_is_403() {
        let secret = "a-long-enough-secret-key";
        let auth = hmac_auth(secret);
        // A timestamp far outside the 300s window.
        let ts = unix_now() - 10_000.0;
        let body = b"{\"cmd\":\"arm\"}";
        let sig = sign(secret, ts, body);
        let headers = RequestHeaders {
            x_timestamp: Some(format!("{ts}")),
            x_nonce: Some("n-expired".to_string()),
            x_hmac_signature: Some(sig),
            ..Default::default()
        };
        assert_eq!(
            auth.decide_hmac(&Method::POST, "/api/command", &headers, body),
            reject(StatusCode::FORBIDDEN, BodyField::Error, messages::REPLAY),
        );
    }

    #[test]
    fn hmac_duplicate_nonce_is_403() {
        let secret = "a-long-enough-secret-key";
        let auth = hmac_auth(secret);
        let ts = unix_now();
        let body = b"{\"cmd\":\"land\"}";
        let sig = sign(secret, ts, body);
        let headers = RequestHeaders {
            x_timestamp: Some(format!("{ts}")),
            x_nonce: Some("dup-nonce".to_string()),
            x_hmac_signature: Some(sig),
            ..Default::default()
        };
        // First passes.
        assert_eq!(
            auth.decide_hmac(&Method::POST, "/api/command", &headers, body),
            Decision::Accept
        );
        // The replay of the same nonce is rejected.
        assert_eq!(
            auth.decide_hmac(&Method::POST, "/api/command", &headers, body),
            reject(StatusCode::FORBIDDEN, BodyField::Error, messages::REPLAY),
        );
    }

    #[test]
    fn hmac_bad_signature_is_401() {
        let secret = "a-long-enough-secret-key";
        let auth = hmac_auth(secret);
        let ts = unix_now();
        let headers = RequestHeaders {
            x_timestamp: Some(format!("{ts}")),
            x_nonce: Some("n-badsig".to_string()),
            x_hmac_signature: Some("00".repeat(32)), // wrong but valid hex
            ..Default::default()
        };
        assert_eq!(
            auth.decide_hmac(&Method::POST, "/api/command", &headers, b"{}"),
            reject(
                StatusCode::UNAUTHORIZED,
                BodyField::Error,
                messages::INVALID_SIGNATURE
            ),
        );
    }

    #[test]
    fn hmac_valid_signature_accepts() {
        let secret = "a-long-enough-secret-key";
        let auth = hmac_auth(secret);
        let ts = unix_now();
        let body = b"{\"cmd\":\"takeoff\",\"alt\":10}";
        let sig = sign(secret, ts, body);
        let headers = RequestHeaders {
            x_timestamp: Some(format!("{ts}")),
            x_nonce: Some("n-valid".to_string()),
            x_hmac_signature: Some(sig),
            ..Default::default()
        };
        assert_eq!(
            auth.decide_hmac(&Method::POST, "/api/command", &headers, body),
            Decision::Accept
        );
    }

    #[test]
    fn hmac_wire_format_is_be_f64_then_body() {
        // Lock the exact wire format against a hand-checked vector: the message
        // is the 8-byte big-endian IEEE-754 double of the timestamp followed by
        // the raw body. A signature computed exactly that way verifies; one over
        // little-endian or body-first does not.
        let secret = b"a-long-enough-secret-key";
        let ts: f64 = 1_700_000_000.5;
        let body = b"payload-bytes";

        let mut be = HmacSha256::new_from_slice(secret).unwrap();
        be.update(&ts.to_be_bytes());
        be.update(body);
        let be_hex = hex::encode(be.finalize().into_bytes());
        assert!(verify_hmac(secret, ts, body, &be_hex));

        // Little-endian timestamp bytes must NOT verify.
        let mut le = HmacSha256::new_from_slice(secret).unwrap();
        le.update(&ts.to_le_bytes());
        le.update(body);
        let le_hex = hex::encode(le.finalize().into_bytes());
        assert!(!verify_hmac(secret, ts, body, &le_hex));

        // Body-before-timestamp must NOT verify.
        let mut swapped = HmacSha256::new_from_slice(secret).unwrap();
        swapped.update(body);
        swapped.update(&ts.to_be_bytes());
        let swapped_hex = hex::encode(swapped.finalize().into_bytes());
        assert!(!verify_hmac(secret, ts, body, &swapped_hex));
    }

    #[test]
    fn replay_detector_window_and_dedup() {
        let det = ReplayDetector::new(300.0, 50_000);
        let now = 1_000_000.0;
        // In-window, first sight → fresh.
        assert!(det.check_at(now, "a", now));
        // Same nonce → rejected.
        assert!(!det.check_at(now, "a", now));
        // A new nonce in-window → fresh.
        assert!(det.check_at(now - 100.0, "b", now));
        // Out of window (older than 300s) → rejected.
        assert!(!det.check_at(now - 1000.0, "c", now));
        // Out of window in the FUTURE (clock skew) → also rejected (abs()).
        assert!(!det.check_at(now + 1000.0, "d", now));
    }
}
