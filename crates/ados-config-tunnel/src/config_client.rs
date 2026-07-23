//! The localhost `/api/config` client the drone-side terminator proxies to.
//!
//! The terminator does NOT reimplement the config surface (the redaction,
//! Pydantic validation, and persist-to-`config.yaml` all live in the Python
//! handler behind the native front). It makes an ordinary on-box HTTP call to
//! [`crate::paths::LOCAL_CONFIG_BASE_URL`]`/api/config`, trusted as a loopback
//! caller, and relays the JSON body verbatim.
//!
//! The trait exists so the terminator's logic is testable without a live
//! `:8080`; [`HttpConfigClient`] is the real implementation.

use async_trait::async_trait;

/// The outcome of a config-surface HTTP call that COMPLETED (any HTTP status).
/// A non-2xx (e.g. a 422 validation reject, or a 404/501 on a headless
/// Python-free node) is a real response to relay honestly, not a client error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// The config surface could not be reached at all (connection refused /
/// timeout / DNS) — distinct from a completed non-2xx response. The terminator
/// reports this as an honest "config surface unavailable", never a fabricated
/// value.
#[derive(Debug, Clone, thiserror::Error)]
#[error("config surface unreachable: {0}")]
pub struct Unreachable(pub String);

#[async_trait]
pub trait ConfigClient: Send + Sync {
    /// `GET /api/config`.
    async fn get(&self) -> Result<ConfigResponse, Unreachable>;
    /// `PUT /api/config` with body `{key, value}`.
    async fn put(&self, key: &str, value: &str) -> Result<ConfigResponse, Unreachable>;
}

/// The real client: blocking `ureq` to `http://127.0.0.1:8080/api/config`,
/// run on a blocking pool so it never stalls the async loop.
pub struct HttpConfigClient {
    base_url: String,
}

impl HttpConfigClient {
    #[must_use]
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
        }
    }
}

/// Map a `ureq` call result to either a completed [`ConfigResponse`] (2xx AND
/// a status error alike) or an [`Unreachable`] transport failure.
fn map_ureq(result: Result<ureq::Response, ureq::Error>) -> Result<ConfigResponse, Unreachable> {
    match result {
        Ok(resp) => Ok(read_response(resp)),
        // A non-2xx status is a real HTTP response — relay it, do not treat it
        // as a transport failure.
        Err(ureq::Error::Status(_code, resp)) => Ok(read_response(resp)),
        Err(ureq::Error::Transport(t)) => Err(Unreachable(t.to_string())),
    }
}

fn read_response(resp: ureq::Response) -> ConfigResponse {
    let status = resp.status();
    let mut body = Vec::new();
    // Bound the read so a runaway upstream cannot exhaust memory; the config
    // surface bodies are small.
    use std::io::Read;
    let _ = resp.into_reader().take(256 * 1024).read_to_end(&mut body);
    ConfigResponse { status, body }
}

#[async_trait]
impl ConfigClient for HttpConfigClient {
    async fn get(&self) -> Result<ConfigResponse, Unreachable> {
        let url = format!("{}/api/config", self.base_url);
        tokio::task::spawn_blocking(move || map_ureq(ureq::get(&url).call()))
            .await
            .map_err(|e| Unreachable(format!("join error: {e}")))?
    }

    async fn put(&self, key: &str, value: &str) -> Result<ConfigResponse, Unreachable> {
        let url = format!("{}/api/config", self.base_url);
        let payload = serde_json::json!({ "key": key, "value": value });
        tokio::task::spawn_blocking(move || map_ureq(ureq::put(&url).send_json(payload)))
            .await
            .map_err(|e| Unreachable(format!("join error: {e}")))?
    }
}
