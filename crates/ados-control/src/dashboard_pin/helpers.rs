//! Dashboard PIN document read, hashing and lockout helpers.

use super::*;

/// Read and parse the record. A missing, unreadable or unparseable file, or a
/// record with no hash/salt (a truncated / cleared file that somehow parsed),
/// is no PIN.
pub(super) fn read_doc(path: &Path) -> Option<DashboardPinDoc> {
    let text = std::fs::read_to_string(path).ok()?;
    let doc: DashboardPinDoc = serde_json::from_str(&text).ok()?;
    (!doc.pin_hash.is_empty() && !doc.salt.is_empty()).then_some(doc)
}

/// Hex `SHA-256(salt || pin)`. See the module threat-model note on why a plain
/// salted SHA-256 (not a slow KDF) is the right choice for a 4-digit PIN.
pub(super) fn hash_pin(salt: &[u8], pin: &str) -> String {
    let mut h = Sha256::new();
    h.update(salt);
    h.update(pin.as_bytes());
    hex::encode(h.finalize())
}

/// A PIN is 4–12 ASCII digits.
pub(super) fn is_valid_pin(pin: &str) -> bool {
    (MIN_PIN_LEN..=MAX_PIN_LEN).contains(&pin.len()) && pin.bytes().all(|b| b.is_ascii_digit())
}

/// The lockout window (seconds) for a given consecutive-wrong count: 0 below the
/// threshold, then an escalating ladder. Re-armed on each wrong attempt past the
/// threshold, so 5–9 wrong throttle at 30 s, 10–14 at 5 min, 15+ at 30 min —
/// enough to make an online 10⁴-space brute force infeasible.
pub(super) fn lock_seconds(failed: u32) -> f64 {
    if failed >= 15 {
        1800.0
    } else if failed >= 10 {
        300.0
    } else if failed >= LOCK_AFTER {
        30.0
    } else {
        0.0
    }
}
