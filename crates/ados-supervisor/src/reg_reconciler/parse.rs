//! Pure parsers for the regulatory reconciler: the global domain out of
//! `iw reg get`, and the wanted domain's rules out of the wireless-regdb
//! `regulatory.db` image. Pure, so the parsing is unit-tested without shelling
//! `iw` or reading `/lib/firmware`. Gated to Linux + test (the OS edges that
//! drive them are Linux-only).

#![cfg(any(target_os = "linux", test))]

/// Parse the global regulatory country from `iw reg get` output: the first
/// `country XX:` line (before any per-phy self-managed block). Returns the
/// uppercase two-character code, or `None`. Pure.
pub(super) fn parse_global_reg_domain(text: &str) -> Option<String> {
    for line in text.lines() {
        let s = line.trim();
        if let Some(rest) = s.strip_prefix("country ") {
            let cc: String = rest.chars().take(2).collect();
            if cc.len() == 2 {
                return Some(cc.to_ascii_uppercase());
            }
        }
    }
    None
}

/// `regulatory.db` magic (`RGDB`) and the one format version the kernel loads.
const REGDB_MAGIC: u32 = 0x5247_4442;
const REGDB_VERSION: u32 = 20;
/// Rule flags that forbid an injection radio from radiating on a channel:
/// no initiating radiation, and radar detection (DFS).
const REGDB_FLAG_DFS: u8 = 1 << 2;
const REGDB_FLAG_NO_IR: u8 = 1 << 3;

/// Centre frequency in kHz of a WiFi channel number (2.4 GHz 1-14, 5 GHz).
fn channel_center_khz(channel: u8) -> Option<u32> {
    let mhz = match channel {
        1..=13 => 2407 + 5 * u32::from(channel),
        14 => 2484,
        32..=196 => 5000 + 5 * u32::from(channel),
        _ => return None,
    };
    Some(mhz * 1000)
}

/// Whether `country`'s rules in a wireless-regdb `regulatory.db` image let a
/// transmitter use the 20 MHz `channel`: some rule covers it and the rule is
/// neither no-IR nor DFS (the same channels `iw phy channels` would list as
/// usable under that country). `None` when the image is malformed, the
/// country is not in it, or the channel number is unknown. Pure: it judges
/// the WANTED domain from the database, never the live per-phy state.
///
/// Layout (big-endian, offsets are `ptr << 2`): a `magic`/`version` header,
/// then `{alpha2[2], coll_ptr u16}` country entries ending at `coll_ptr == 0`;
/// a collection is `{len, n_rules, dfs_region}` followed (at `len` rounded up
/// to 2) by `n_rules` u16 rule pointers; a rule is
/// `{len, flags, max_eirp u16, start u32, end u32, max_bw u32}` in kHz.
pub(super) fn regdb_permits_channel(db: &[u8], country: &str, channel: u8) -> Option<bool> {
    let u8_at = |off: usize| db.get(off).copied();
    let u16_at = |off: usize| {
        db.get(off..off + 2)
            .map(|b| u16::from_be_bytes([b[0], b[1]]))
    };
    let u32_at = |off: usize| {
        db.get(off..off + 4)
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    };
    if u32_at(0)? != REGDB_MAGIC || u32_at(4)? != REGDB_VERSION {
        return None;
    }
    let want = country.trim().to_ascii_uppercase();
    let want = want.as_bytes();
    if want.len() != 2 {
        return None;
    }
    let center = channel_center_khz(channel)?;
    let (low, high) = (center - 10_000, center + 10_000);

    let mut entry = 8;
    let coll = loop {
        let ptr = usize::from(u16_at(entry + 2)?);
        if ptr == 0 {
            return None;
        }
        if db.get(entry..entry + 2)? == want {
            break ptr << 2;
        }
        entry += 4;
    };
    let coll_len = usize::from(u8_at(coll)?);
    let n_rules = usize::from(u8_at(coll + 1)?);
    let rules_at = coll + coll_len.div_ceil(2) * 2;
    for i in 0..n_rules {
        let rule = usize::from(u16_at(rules_at + 2 * i)?) << 2;
        let flags = u8_at(rule + 1)?;
        let start = u32_at(rule + 4)?;
        let end = u32_at(rule + 8)?;
        if start <= low && high <= end {
            return Some(flags & (REGDB_FLAG_NO_IR | REGDB_FLAG_DFS) == 0);
        }
    }
    Some(false)
}

#[cfg(test)]
mod tests;
