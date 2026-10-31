//! The receiver's accept window: its six-digit code, the wall-clock times the
//! operator sees, the monotonic deadline that decides freshness, the pending
//! join requests and the approvals.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::crypto::INVITE_CODE_DIGITS;

/// A relay's join request waiting for operator approval.
#[derive(Debug, Clone)]
pub struct PendingRequest {
    pub device_id: String,
    pub relay_pubkey: [u8; 32],
    pub remote_addr: SocketAddr,
    pub received_at_ms: i64,
}

/// One open accept window.
#[derive(Debug, Clone)]
pub struct AcceptWindow {
    /// Wall-clock open time, for display only.
    pub opened_at_ms: i64,
    /// Wall-clock close time, for display only.
    pub closes_at_ms: i64,
    /// The code bound into every invite sealed in this window.
    pub code: String,
    /// The authoritative deadline: an NTP step cannot move it.
    pub deadline: Instant,
    pub pending: Vec<PendingRequest>,
    /// Approved device id → wall-clock approval time.
    pub approvals: BTreeMap<String, i64>,
}

impl AcceptWindow {
    /// A window opened now for `duration_s` seconds.
    pub fn open(now_ms: i64, duration_s: u64) -> Self {
        Self {
            opened_at_ms: now_ms,
            closes_at_ms: now_ms + (duration_s as i64) * 1000,
            code: new_invite_code(),
            deadline: Instant::now() + Duration::from_secs(duration_s),
            pending: Vec::new(),
            approvals: BTreeMap::new(),
        }
    }

    /// Whether the deadline is still ahead.
    pub fn is_open(&self) -> bool {
        Instant::now() < self.deadline
    }

    /// The pending request for `device_id`.
    pub fn pending_for(&self, device_id: &str) -> Option<&PendingRequest> {
        self.pending.iter().find(|r| r.device_id == device_id)
    }

    /// The `snapshot` body for this window.
    pub fn snapshot(&self) -> Value {
        let pending: Vec<Value> = self
            .pending
            .iter()
            .map(|r| {
                json!({
                    "device_id": r.device_id,
                    "received_at_ms": r.received_at_ms,
                    "remote_ip": r.remote_addr.ip().to_string(),
                })
            })
            .collect();
        json!({
            "open": self.is_open(),
            "opened_at_ms": self.opened_at_ms,
            "closes_at_ms": self.closes_at_ms,
            "pending": pending,
            "approvals": self.approvals,
            "code": self.code,
        })
    }
}

/// The snapshot when no window exists.
pub fn closed_snapshot() -> Value {
    json!({"open": false})
}

/// A fresh accept-window code: six uniformly random decimal digits.
pub fn new_invite_code() -> String {
    const SPAN: u32 = 1_000_000;
    // Reject the top partial range so every code is equally likely.
    const LIMIT: u32 = u32::MAX - (u32::MAX % SPAN);
    loop {
        let mut buf = [0u8; 4];
        getrandom::fill(&mut buf).expect("OS RNG for the window code");
        let n = u32::from_le_bytes(buf);
        if n < LIMIT {
            return format!("{:0width$}", n % SPAN, width = INVITE_CODE_DIGITS);
        }
    }
}

/// Parse one join datagram, `{"type":"join","device_id","pubkey_hex"}` with a
/// 32-byte key. Anything else is `None` and dropped.
pub fn parse_join(datagram: &[u8]) -> Option<(String, [u8; 32])> {
    let msg: Value = serde_json::from_slice(datagram).ok()?;
    if msg.get("type").and_then(Value::as_str) != Some("join") {
        return None;
    }
    let device_id = msg.get("device_id").and_then(Value::as_str)?;
    let pubkey_hex = msg.get("pubkey_hex").and_then(Value::as_str)?;
    if device_id.is_empty() {
        return None;
    }
    let pubkey: [u8; 32] = hex::decode(pubkey_hex).ok()?.try_into().ok()?;
    Some((device_id.to_string(), pubkey))
}
