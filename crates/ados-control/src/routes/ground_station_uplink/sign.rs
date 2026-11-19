//! The uplink request MAC, both directions, and the box's take-back call to the
//! holding phone.
//!
//! `mac = HMAC-SHA256(uplink key, "ados/gs-uplink/v1\n" || ts_ms || "\n" || body)`,
//! base64, where `ts_ms` is the decimal unix-millisecond timestamp the sender
//! puts in [`TS_HEADER`] and `body` is the exact request body bytes. A request
//! is accepted within [`CLOCK_WINDOW_MS`] of the receiver's clock and only with
//! a timestamp later than the last one accepted from the same phone, so a
//! captured request cannot be replayed.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use base64::Engine as _;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Domain separation for the uplink MAC.
pub const MAC_CONTEXT: &[u8] = b"ados/gs-uplink/v1\n";
/// The approved phone's X25519 public key, base64.
pub const PHONE_KEY_HEADER: &str = "x-ados-phone-key";
/// The sender's unix-millisecond timestamp.
pub const TS_HEADER: &str = "x-ados-uplink-ts";
/// The base64 MAC.
pub const MAC_HEADER: &str = "x-ados-uplink-mac";
/// How far the sender's clock may sit from the receiver's.
pub const CLOCK_WINDOW_MS: i64 = 120_000;
/// How long the box waits for the phone to confirm a take-back.
pub const YIELD_TIMEOUT: Duration = Duration::from_secs(2);

/// The MAC over `ts_ms` and `body` under `key`.
pub fn mac(key: &[u8; 32], ts_ms: i64, body: &[u8]) -> [u8; 32] {
    let mut m = <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC takes any key length");
    m.update(MAC_CONTEXT);
    m.update(ts_ms.to_string().as_bytes());
    m.update(b"\n");
    m.update(body);
    m.finalize().into_bytes().into()
}

/// Why a phone's request was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacError {
    Missing,
    UnknownPhone,
    Clock,
    Replay,
    Mismatch,
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
        }
    }
}

/// Verify `mac_b64` over `ts_ms` and `body`, in constant time.
pub fn verify(key: &[u8; 32], ts_ms: i64, body: &[u8], mac_b64: &str) -> bool {
    let Ok(got) = base64::engine::general_purpose::STANDARD.decode(mac_b64.trim()) else {
        return false;
    };
    crate::auth::constant_time_eq(&got, &mac(key, ts_ms, body))
}

/// The last timestamp accepted per phone fingerprint.
#[derive(Debug, Default)]
pub struct ReplayGuard {
    last: HashMap<String, i64>,
}

impl ReplayGuard {
    /// Accept `ts_ms` from `phone` once: inside the clock window and later than
    /// anything accepted from it before.
    pub fn admit(&mut self, phone: &str, ts_ms: i64, now_ms: i64) -> Result<(), MacError> {
        if (ts_ms - now_ms).abs() > CLOCK_WINDOW_MS {
            return Err(MacError::Clock);
        }
        if self.last.get(phone).is_some_and(|&last| ts_ms <= last) {
            return Err(MacError::Replay);
        }
        self.last.insert(phone.to_string(), ts_ms);
        Ok(())
    }
}

/// Tell the holding phone to stop transmitting: one signed `POST` to the
/// address it registered at take. True when it answered 2xx within
/// [`YIELD_TIMEOUT`], which it does only after its transmitters stopped.
pub async fn call_yield(url: &str, key: &[u8; 32], box_device_id: &str, now_ms: i64) -> bool {
    let Some((addr, path)) = parse_http_url(url) else {
        return false;
    };
    let body = serde_json::json!({"op": "yield", "box_device_id": box_device_id}).to_string();
    let mac_b64 =
        base64::engine::general_purpose::STANDARD.encode(mac(key, now_ms, body.as_bytes()));
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
