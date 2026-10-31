//! Secret-key detection and value redaction.

use super::*;

/// True when `key` marks a secret-bearing field: a case-insensitive match where
/// the lower-cased key ends with, or exactly equals, one of [`SECRET_SUFFIXES`].
pub fn is_secret_key(key: &str) -> bool {
    let kl = key.to_ascii_lowercase();
    SECRET_SUFFIXES.iter().any(|s| kl.ends_with(s) || kl == *s)
}

/// Redact one string value for the given key.
///
/// Mirrors the agent's structured-logging redaction byte-for-byte:
///
/// - if `key` is not secret-bearing, the value is returned unchanged;
/// - if the value is empty, it is returned unchanged;
/// - if the value already carries the [`REDACT_PREFIX`] sentinel, it is returned
///   unchanged (idempotent — a value that traverses the chain twice is not
///   redacted again);
/// - otherwise the value is replaced with `redacted:len=<N>`, `N` its length in
///   Unicode characters (Python's `len(v)`).
///
/// Nothing computed from the content survives. A plaintext head plus an unkeyed
/// digest, which this used to emit, lets anyone who can read the store recover
/// a short secret (a six-digit pairing code has two unknown digits left) by
/// hashing candidates.
pub fn redact(key: &str, value: &str) -> String {
    if value.is_empty() || value.starts_with(REDACT_PREFIX) {
        return value.to_string();
    }
    if !is_secret_key(key) {
        return value.to_string();
    }
    format!("{REDACT_PREFIX}len={}", value.chars().count())
}

/// Redact every secret-bearing string field of an open map in place. Non-string
/// values, empty strings, and already-redacted values are left untouched.
/// Returns `true` if at least one field value was changed.
pub fn redact_map(fields: &mut Fields) -> bool {
    let mut changed = false;
    for (k, v) in fields.iter_mut() {
        if let rmpv::Value::String(s) = v {
            if let Some(text) = s.as_str() {
                let redacted = redact(k, text);
                if redacted != text {
                    *v = rmpv::Value::from(redacted);
                    changed = true;
                }
            }
        }
    }
    changed
}

// --- explicit window export / mark-synced types -------------------------
