//! Keyset cursor encoding for stable pagination.
//!
//! Pages are keyset-paginated, not offset-paginated: the cursor carries the
//! last `(ts_us, id)` of the page and the next page selects rows strictly
//! before that boundary (newest first). Keyset paging is O(log n) per page
//! regardless of depth and is stable under concurrent inserts, which an
//! append-heavy store has constantly.
//!
//! The cursor is opaque: a URL-safe base64 of a small msgpack value carrying
//! the boundary plus a fingerprint of the filter set it was issued against. A
//! cursor replayed against a different filter set fails the fingerprint check
//! and is rejected with a stable `bad_cursor` error rather than silently
//! returning rows from a scan that no longer matches what the caller asked for.

use base64::Engine;
use serde::{Deserialize, Serialize};

/// The decoded keyset boundary plus the filter fingerprint it was issued for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    /// The `ts_us` of the last row on the issuing page.
    #[serde(rename = "t")]
    pub ts_us: i64,
    /// The primary-key `id` of the last row on the issuing page (the tiebreak
    /// when two rows share a `ts_us`).
    #[serde(rename = "i")]
    pub id: i64,
    /// A fingerprint of the filter set the cursor was issued against. A cursor
    /// presented with a different filter set is rejected.
    #[serde(rename = "f")]
    pub fingerprint: u64,
}

/// Errors decoding a cursor presented by a client.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CursorError {
    /// The token did not base64-decode.
    #[error("cursor is not valid base64")]
    Base64,
    /// The decoded bytes did not parse as the cursor msgpack shape.
    #[error("cursor does not parse")]
    Decode,
    /// The cursor was issued against a different filter set.
    #[error("cursor does not match the current filter set")]
    Fingerprint,
}

impl Cursor {
    /// Build a cursor for a `(ts_us, id)` boundary under a filter fingerprint.
    pub fn new(ts_us: i64, id: i64, fingerprint: u64) -> Self {
        Self {
            ts_us,
            id,
            fingerprint,
        }
    }

    /// Encode to the opaque URL-safe token returned in `page.next_cursor`.
    pub fn encode(&self) -> String {
        // The shape is tiny and fixed; msgpack never fails here, but fall back
        // to an empty token rather than panicking if it ever did.
        let body = rmp_serde::to_vec(self).unwrap_or_default();
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(body)
    }

    /// Decode an opaque token and verify it was issued for `fingerprint`.
    pub fn decode(token: &str, fingerprint: u64) -> Result<Self, CursorError> {
        let body = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(token.as_bytes())
            .map_err(|_| CursorError::Base64)?;
        let cursor: Cursor = rmp_serde::from_slice(&body).map_err(|_| CursorError::Decode)?;
        if cursor.fingerprint != fingerprint {
            return Err(CursorError::Fingerprint);
        }
        Ok(cursor)
    }
}

/// A deterministic fingerprint of a filter set. The handler feeds every filter
/// value that shapes the result set (the table, the bounds, the source/metric
/// lists, the level floor, the text and session restrictions) so a cursor is
/// only ever honoured against the exact query it was issued for.
///
/// FNV-1a over the ordered byte segments: cheap, allocation-free, and stable
/// across runs (no random seed), so a cursor round-trips across requests.
#[derive(Debug, Default, Clone)]
pub struct FilterFingerprint {
    state: u64,
}

impl FilterFingerprint {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    /// Start an empty fingerprint accumulator.
    pub fn new() -> Self {
        Self {
            state: Self::FNV_OFFSET,
        }
    }

    fn mix_byte(&mut self, b: u8) {
        self.state ^= u64::from(b);
        self.state = self.state.wrapping_mul(Self::FNV_PRIME);
    }

    /// Fold a raw byte segment into the fingerprint, length-prefixed so two
    /// adjacent segments cannot collide by sharing a boundary.
    pub fn add_bytes(&mut self, bytes: &[u8]) -> &mut Self {
        for b in (bytes.len() as u64).to_le_bytes() {
            self.mix_byte(b);
        }
        for &b in bytes {
            self.mix_byte(b);
        }
        self
    }

    /// Fold a string segment in.
    pub fn add_str(&mut self, s: &str) -> &mut Self {
        self.add_bytes(s.as_bytes())
    }

    /// Fold an optional string segment in, distinguishing absent from empty.
    pub fn add_opt_str(&mut self, s: Option<&str>) -> &mut Self {
        match s {
            Some(s) => {
                self.mix_byte(1);
                self.add_str(s);
            }
            None => self.mix_byte(0),
        }
        self
    }

    /// Fold an integer segment in.
    pub fn add_i64(&mut self, n: i64) -> &mut Self {
        self.add_bytes(&n.to_le_bytes())
    }

    /// Fold an optional integer segment in, distinguishing absent from a value.
    pub fn add_opt_i64(&mut self, n: Option<i64>) -> &mut Self {
        match n {
            Some(n) => {
                self.mix_byte(1);
                self.add_i64(n);
            }
            None => self.mix_byte(0),
        }
        self
    }

    /// Fold a list of string segments in (order matters; the caller sorts when
    /// the filter is order-insensitive so the fingerprint is stable).
    pub fn add_str_list(&mut self, items: &[String]) -> &mut Self {
        self.add_i64(items.len() as i64);
        for item in items {
            self.add_str(item);
        }
        self
    }

    /// Finalize the accumulated fingerprint.
    pub fn finish(&self) -> u64 {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_round_trips_under_a_matching_fingerprint() {
        let c = Cursor::new(1_700_000_000_000_000, 42, 0xDEAD_BEEF);
        let token = c.encode();
        let back = Cursor::decode(&token, 0xDEAD_BEEF).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn cursor_is_rejected_under_a_different_fingerprint() {
        let c = Cursor::new(1, 2, 0xAAAA);
        let token = c.encode();
        let err = Cursor::decode(&token, 0xBBBB).unwrap_err();
        assert_eq!(err, CursorError::Fingerprint);
    }

    #[test]
    fn a_garbage_token_is_rejected_cleanly() {
        assert_eq!(
            Cursor::decode("!!!not base64!!!", 0).unwrap_err(),
            CursorError::Base64
        );
        // Valid base64 that is not the cursor shape.
        let not_a_cursor = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"hello");
        assert_eq!(
            Cursor::decode(&not_a_cursor, 0).unwrap_err(),
            CursorError::Decode
        );
    }

    #[test]
    fn fingerprint_is_stable_and_order_sensitive() {
        let mut a = FilterFingerprint::new();
        a.add_str("logs")
            .add_opt_i64(Some(100))
            .add_str_list(&["api".into(), "video".into()]);
        let mut b = FilterFingerprint::new();
        b.add_str("logs")
            .add_opt_i64(Some(100))
            .add_str_list(&["api".into(), "video".into()]);
        assert_eq!(
            a.finish(),
            b.finish(),
            "same inputs give the same fingerprint"
        );

        let mut c = FilterFingerprint::new();
        c.add_str("logs")
            .add_opt_i64(Some(100))
            .add_str_list(&["video".into(), "api".into()]);
        assert_ne!(
            a.finish(),
            c.finish(),
            "reordered list changes the fingerprint"
        );
    }

    #[test]
    fn fingerprint_distinguishes_absent_from_present() {
        let mut a = FilterFingerprint::new();
        a.add_opt_i64(None);
        let mut b = FilterFingerprint::new();
        b.add_opt_i64(Some(0));
        assert_ne!(
            a.finish(),
            b.finish(),
            "absent must differ from a zero value"
        );
    }

    #[test]
    fn length_prefix_prevents_boundary_collisions() {
        let mut a = FilterFingerprint::new();
        a.add_str("ab").add_str("c");
        let mut b = FilterFingerprint::new();
        b.add_str("a").add_str("bc");
        assert_ne!(a.finish(), b.finish(), "split point must matter");
    }
}
