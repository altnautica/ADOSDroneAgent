//! The uplink request MACs, one per direction, the box's replay guard, and
//! the box's take-back call to the holding phone.
//!
//! `mac = HMAC-SHA256(uplink key, context || ts_ms || "\n" || body)`, base64,
//! where `ts_ms` is the decimal unix-millisecond timestamp the sender puts in
//! [`TS_HEADER`] and `body` is the exact request body bytes. The context names
//! the direction: [`MAC_CONTEXT`] for a phone's take and give-back to the box,
//! [`YIELD_MAC_CONTEXT`] for the box's take-back call to the phone, so a
//! phone's own signed request can never pass as a take-back.
//!
//! A request is accepted within [`CLOCK_WINDOW_MS`] of the receiver's clock
//! and only with a timestamp later than the last one accepted from the same
//! phone, so a captured request cannot be replayed. The box keeps those
//! timestamps on disk ([`replay_path`]), so a restart of this service does not
//! reopen the window.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use axum::http::StatusCode;
use base64::Engine as _;
use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
use sha2::Sha256;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Domain separation for a phone's requests to the box (take, give-back).
pub const MAC_CONTEXT: &[u8] = b"ados/gs-uplink/v1\n";
/// Domain separation for the box's take-back call to the phone.
pub const YIELD_MAC_CONTEXT: &[u8] = b"ados/gs-uplink-yield/v1\n";
/// The approved phone's X25519 public key, base64.
pub const PHONE_KEY_HEADER: &str = "x-ados-phone-key";
/// The sender's unix-millisecond timestamp.
pub const TS_HEADER: &str = "x-ados-uplink-ts";
/// The base64 MAC.
pub const MAC_HEADER: &str = "x-ados-uplink-mac";
/// How far the sender's clock may sit from the receiver's.
pub const CLOCK_WINDOW_MS: i64 = 120_000;
/// How long the box waits for the phone to confirm a take-back. The phone
/// answers only once its own radio has read quiet for a second, which takes
/// it up to five; the box starts its transmitters after the answer, or after
/// this window without one, so it covers the phone's worst case plus the
/// LAN round trip and the box never starts on top of a phone still stopping.
pub const YIELD_TIMEOUT: Duration = Duration::from_secs(6);
/// The most phones the replay guard remembers; the one heard from longest ago
/// goes first. Only a phone whose MAC verified reaches the guard, and at most
/// [`super::phones::MAX_PHONES`] are approved at once, so this covers every
/// phone that can still sign plus those approved and replaced before them.
pub const REPLAY_KEPT: usize = 64;

/// A phone's request MAC over `ts_ms` and `body` under `key`.
pub fn mac(key: &[u8; 32], ts_ms: i64, body: &[u8]) -> [u8; 32] {
    mac_in(MAC_CONTEXT, key, ts_ms, body)
}

/// The take-back call's MAC over `ts_ms` and `body` under `key`.
pub fn yield_mac(key: &[u8; 32], ts_ms: i64, body: &[u8]) -> [u8; 32] {
    mac_in(YIELD_MAC_CONTEXT, key, ts_ms, body)
}

fn mac_in(context: &[u8], key: &[u8; 32], ts_ms: i64, body: &[u8]) -> [u8; 32] {
    let mut m = <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC takes any key length");
    m.update(context);
    m.update(ts_ms.to_string().as_bytes());
    m.update(b"\n");
    m.update(body);
    m.finalize().into_bytes().into()
}

/// The take-back call's body; the phone checks both fields.
pub fn yield_body(box_device_id: &str) -> String {
    serde_json::json!({"op": "yield", "box_device_id": box_device_id}).to_string()
}

/// Why a phone's request was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacError {
    Missing,
    UnknownPhone,
    Clock,
    Replay,
    Mismatch,
    /// The accepted timestamp could not be kept on disk.
    Unrecorded,
}

impl MacError {
    pub fn detail(self) -> &'static str {
        match self {
            MacError::Missing => "The request carries no phone key, timestamp or MAC.",
            MacError::UnknownPhone => {
                "This phone holds no ground-role invite from this ground station."
            }
            MacError::Clock => {
                "The phone's clock is more than two minutes from this ground station's."
            }
            MacError::Replay => {
                "The request repeats or predates one already accepted from this phone."
            }
            MacError::Mismatch => {
                "The request's MAC does not verify under this phone's uplink key."
            }
            MacError::Unrecorded => {
                "This ground station could not record the request's timestamp, so it did not act on it."
            }
        }
    }

    /// The route's error code.
    pub fn code(self) -> &'static str {
        match self {
            MacError::Unrecorded => "E_UPLINK_REPLAY_STORE",
            _ => "E_UPLINK_UNAUTHORIZED",
        }
    }

    /// The route's status: a fault on this box is not the caller's.
    pub fn status(self) -> StatusCode {
        match self {
            MacError::Unrecorded => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::UNAUTHORIZED,
        }
    }
}

/// Verify a phone's `mac_b64` over `ts_ms` and `body`, in constant time.
pub fn verify(key: &[u8; 32], ts_ms: i64, body: &[u8], mac_b64: &str) -> bool {
    let Ok(got) = base64::engine::general_purpose::STANDARD.decode(mac_b64.trim()) else {
        return false;
    };
    crate::auth::constant_time_eq(&got, &mac(key, ts_ms, body))
}

/// Where the route keeps its replay guard: beside the approved phones.
pub fn replay_path() -> PathBuf {
    super::phones::phones_path().with_file_name("uplink-replay.json")
}

/// The replay guard's file.
#[derive(Debug, Default, Deserialize)]
struct ReplayFile {
    #[serde(default)]
    last: BTreeMap<String, i64>,
}

/// The last timestamp accepted per phone fingerprint.
#[derive(Debug, Default)]
pub struct ReplayGuard {
    last: BTreeMap<String, i64>,
    /// Where every acceptance is written, mode 0600; `None` keeps the guard
    /// in memory only.
    path: Option<PathBuf>,
}

impl ReplayGuard {
    /// The guard kept at `path`. An absent file has accepted nothing. An
    /// unreadable one reads as empty, with an error in the journal: it is
    /// written atomically and only after a MAC verified, so only a fault on
    /// this box can spoil it.
    pub fn load(path: PathBuf) -> Self {
        let last = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str::<ReplayFile>(&text)
                .map(|f| f.last)
                .unwrap_or_else(|e| {
                    tracing::error!(error = %e, path = %path.display(), "uplink_replay_guard_unreadable");
                    BTreeMap::new()
                }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => {
                tracing::error!(error = %e, path = %path.display(), "uplink_replay_guard_unreadable");
                BTreeMap::new()
            }
        };
        Self {
            last,
            path: Some(path),
        }
    }

    /// Accept `ts_ms` from `phone` once: inside the clock window and later than
    /// anything accepted from it before. A guard kept on disk accepts only once
    /// the timestamp is written there.
    pub fn admit(&mut self, phone: &str, ts_ms: i64, now_ms: i64) -> Result<(), MacError> {
        if (ts_ms - now_ms).abs() > CLOCK_WINDOW_MS {
            return Err(MacError::Clock);
        }
        if self.last.get(phone).is_some_and(|&last| ts_ms <= last) {
            return Err(MacError::Replay);
        }
        self.last.insert(phone.to_string(), ts_ms);
        while self.last.len() > REPLAY_KEPT {
            let oldest = self
                .last
                .iter()
                .min_by_key(|(_, ts)| **ts)
                .map(|(fp, _)| fp.clone());
            if let Some(fp) = oldest {
                self.last.remove(&fp);
            }
        }
        self.persist()
    }

    fn persist(&self) -> Result<(), MacError> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let doc = serde_json::json!({ "last": self.last });
        ados_protocol::sidecar::write_json_atomic(path, &doc, 0o600).map_err(|e| {
            tracing::error!(error = %e, path = %path.display(), "uplink_replay_guard_not_stored");
            MacError::Unrecorded
        })
    }
}

/// The take-back call's body and its base64 MAC under `key` at `now_ms`, the
/// timestamp the call carries in [`TS_HEADER`].
pub fn yield_request(key: &[u8; 32], box_device_id: &str, now_ms: i64) -> (String, String) {
    let body = yield_body(box_device_id);
    let mac_b64 =
        base64::engine::general_purpose::STANDARD.encode(yield_mac(key, now_ms, body.as_bytes()));
    (body, mac_b64)
}

/// Tell the holding phone to stop transmitting: one `POST` to the address it
/// registered at take, signed under [`YIELD_MAC_CONTEXT`]. True when it
/// answered 2xx within [`YIELD_TIMEOUT`], which it does only after its
/// transmitters stopped.
pub async fn call_yield(url: &str, key: &[u8; 32], box_device_id: &str, now_ms: i64) -> bool {
    let Some((addr, path)) = parse_http_url(url) else {
        return false;
    };
    let (body, mac_b64) = yield_request(key, box_device_id, now_ms);
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
         {TS_HEADER}: {now_ms}\r\n{MAC_HEADER}: {mac_b64}\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let exchange = async {
        let mut stream = tokio::net::TcpStream::connect(addr).await.ok()?;
        stream.write_all(request.as_bytes()).await.ok()?;
        let mut head = [0u8; 64];
        let n = stream.read(&mut head).await.ok()?;
        let line = std::str::from_utf8(&head[..n]).ok()?;
        let code: u16 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some((200..300).contains(&code))
    };
    matches!(
        tokio::time::timeout(YIELD_TIMEOUT, exchange).await,
        Ok(Some(true))
    )
}

/// `http://<ip>:<port><path>` → the socket address and the path. Only a
/// literal IP: the address is the one the phone's own request came from.
pub fn parse_http_url(url: &str) -> Option<(SocketAddr, String)> {
    let rest = url.strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_string()),
        None => (rest, "/".to_string()),
    };
    Some((authority.parse().ok()?, path))
}

/// The take-back address for a phone at `ip` listening on `port`.
pub fn yield_url(ip: std::net::IpAddr, port: u16) -> String {
    format!("http://{}/uplink/yield", SocketAddr::new(ip, port))
}
