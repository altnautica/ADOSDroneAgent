//! STATUSTEXT reassembly.
//!
//! A flight controller splits a text longer than one 50-byte STATUSTEXT into
//! chunks that share a non-zero `id` and count up in `chunk_seq`; the last
//! chunk is shorter than 50 bytes or carries a NUL. [`StatusTextAssembler`]
//! joins them so a surface shows one line, not one row per fragment.
//!
//! The dialect this workspace decodes with does not carry the MAVLink 2
//! extension fields (`id`, `chunk_seq`), so [`StatusTextAssembler::push_untagged`]
//! covers that case: a full 50-byte chunk with no NUL starts or continues a
//! text from the same sender at the same severity, and the next shorter chunk
//! completes it.

use std::collections::HashMap;

/// Bytes in one STATUSTEXT text field.
pub const CHUNK_LEN: usize = 50;
/// How long an incomplete text waits for its next chunk before it is flushed.
pub const STALE_MS: u64 = 1000;
/// The id [`StatusTextAssembler::push_untagged`] files its partials under; a
/// real chunk id never reaches it because tagged and untagged keys differ in
/// the `tagged` flag.
const UNTAGGED: u16 = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Key {
    sysid: u8,
    compid: u8,
    id: u16,
    tagged: bool,
}

#[derive(Debug, Clone)]
struct Partial {
    sev: u8,
    chunks: Vec<(u8, String)>,
    last_ms: u64,
}

impl Partial {
    fn text(mut self) -> (u8, String) {
        self.chunks.sort_by_key(|(seq, _)| *seq);
        let text: String = self.chunks.into_iter().map(|(_, t)| t).collect();
        (self.sev, text.trim().to_owned())
    }
}

/// Joins chunked STATUSTEXT per sender and id.
#[derive(Debug, Clone, Default)]
pub struct StatusTextAssembler {
    partial: HashMap<Key, Partial>,
}

/// The text up to the first NUL, and whether the chunk ends the message.
fn decode(text: &[u8; CHUNK_LEN]) -> (String, bool) {
    let end = text.iter().position(|&b| b == 0);
    let body = &text[..end.unwrap_or(CHUNK_LEN)];
    (String::from_utf8_lossy(body).into_owned(), end.is_some())
}

impl StatusTextAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one STATUSTEXT in. Returns `(severity, text)` when a message is
    /// complete: at once for `id == 0`, else on its final chunk.
    #[allow(clippy::too_many_arguments)]
    pub fn push(
        &mut self,
        sysid: u8,
        compid: u8,
        sev: u8,
        text: &[u8; CHUNK_LEN],
        id: u16,
        chunk_seq: u8,
        now_ms: u64,
    ) -> Option<(u8, String)> {
        let (body, ends) = decode(text);
        if id == 0 {
            return Some((sev, body.trim().to_owned()));
        }
        let key = Key {
            sysid,
            compid,
            id,
            tagged: true,
        };
        self.fold(key, sev, chunk_seq, body, ends, now_ms)
    }

    /// Fold one STATUSTEXT whose `id`/`chunk_seq` extensions were not decoded.
    /// Returns every text this chunk completes, oldest first: a pending text
    /// from the same sender at another severity is finished as it stands.
    pub fn push_untagged(
        &mut self,
        sysid: u8,
        compid: u8,
        sev: u8,
        text: &[u8; CHUNK_LEN],
        now_ms: u64,
    ) -> Vec<(u8, String)> {
        let (body, ends) = decode(text);
        let key = Key {
            sysid,
            compid,
            id: UNTAGGED,
            tagged: false,
        };
        let mut out = Vec::new();
        if self.partial.get(&key).is_some_and(|p| p.sev != sev) {
            out.extend(self.partial.remove(&key).map(Partial::text));
        }
        let seq = self.partial.get(&key).map_or(0, |p| p.chunks.len() as u8);
        if seq == 0 && ends {
            out.push((sev, body.trim().to_owned()));
        } else {
            out.extend(self.fold(key, sev, seq, body, ends, now_ms));
        }
        out
    }

    fn fold(
        &mut self,
        key: Key,
        sev: u8,
        seq: u8,
        body: String,
        ends: bool,
        now_ms: u64,
    ) -> Option<(u8, String)> {
        let p = self.partial.entry(key).or_insert(Partial {
            sev,
            chunks: Vec::new(),
            last_ms: now_ms,
        });
        if !p.chunks.iter().any(|(s, _)| *s == seq) {
            p.chunks.push((seq, body));
        }
        p.last_ms = now_ms;
        if ends {
            return self.partial.remove(&key).map(Partial::text);
        }
        None
    }

    /// Texts whose next chunk has not come for [`STALE_MS`], as they stand.
    pub fn flush_stale(&mut self, now_ms: u64) -> Vec<(u8, String)> {
        let stale: Vec<Key> = self
            .partial
            .iter()
            .filter(|(_, p)| now_ms.saturating_sub(p.last_ms) >= STALE_MS)
            .map(|(k, _)| *k)
            .collect();
        stale
            .into_iter()
            .filter_map(|k| self.partial.remove(&k).map(Partial::text))
            .collect()
    }
}

#[cfg(test)]
mod tests;
