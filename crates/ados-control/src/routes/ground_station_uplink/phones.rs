//! The phones a ground-station operator approved through a ground-role invite,
//! each with the uplink key the sealed bundle handed it.
//!
//! The invite delivers its bundle sealed to the phone's X25519 key, so the
//! bundle is already a confidential channel to exactly that phone. The box
//! puts a fresh 32-byte uplink key in it and keeps the same key here, against
//! the phone's public key; every uplink request after that is authenticated
//! with an HMAC under it, in both directions (the phone's take and give-back,
//! the box's take-back call to the phone). An X25519 key can agree a secret
//! but cannot sign, which is why the bundle carries a key rather than the
//! request carrying a signature.
//!
//! Stored at `<wfb key dir>/uplink-phones.json`, mode 0600, at most
//! [`MAX_PHONES`] entries; approving the same phone again replaces its key.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde::{Deserialize, Serialize};

/// The most approved phones kept; the oldest approval goes first.
pub const MAX_PHONES: usize = 8;

/// One approved phone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovedPhone {
    pub phone_pub_b64: String,
    pub fingerprint: String,
    pub label: String,
    pub uplink_key_b64: String,
    pub approved_at_ms: i64,
}

impl ApprovedPhone {
    pub fn uplink_key(&self) -> Option<[u8; 32]> {
        base64::engine::general_purpose::STANDARD
            .decode(&self.uplink_key_b64)
            .ok()?
            .try_into()
            .ok()
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PhonesFile {
    #[serde(default)]
    phones: Vec<ApprovedPhone>,
}

/// The store's path, honouring `ADOS_WFB_KEY_DIR` like the pair routes.
pub fn phones_path() -> PathBuf {
    std::env::var("ADOS_WFB_KEY_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/etc/ados/wfb"))
        .join("uplink-phones.json")
}

/// Every approved phone; an absent or unreadable file is none.
pub fn load(path: &Path) -> Vec<ApprovedPhone> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<PhonesFile>(&t).ok())
        .map(|f| f.phones)
        .unwrap_or_default()
}

/// Keep `phone`, replacing an earlier approval of the same key.
pub fn remember(path: &Path, phone: ApprovedPhone) -> std::io::Result<()> {
    let mut phones = load(path);
    phones.retain(|p| p.phone_pub_b64 != phone.phone_pub_b64);
    phones.push(phone);
    phones.sort_by_key(|p| p.approved_at_ms);
    let excess = phones.len().saturating_sub(MAX_PHONES);
    phones.drain(..excess);
    let doc = serde_json::to_value(PhonesFile { phones }).map_err(std::io::Error::other)?;
    ados_protocol::sidecar::write_json_atomic(path, &doc, 0o600)
}

/// The approved phone with this public key.
pub fn by_pub<'a>(phones: &'a [ApprovedPhone], phone_pub_b64: &str) -> Option<&'a ApprovedPhone> {
    phones
        .iter()
        .find(|p| p.phone_pub_b64 == phone_pub_b64.trim())
}

/// The approved phone with this fingerprint.
pub fn by_fingerprint<'a>(
    phones: &'a [ApprovedPhone],
    fingerprint: &str,
) -> Option<&'a ApprovedPhone> {
    phones.iter().find(|p| p.fingerprint == fingerprint)
}

/// A fresh uplink key from the OS RNG.
pub fn new_uplink_key() -> [u8; 32] {
    let mut key = [0u8; 32];
    getrandom::fill(&mut key).expect("OS RNG for the uplink key");
    key
}
