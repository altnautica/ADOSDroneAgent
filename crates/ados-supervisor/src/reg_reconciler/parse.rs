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
mod tests {
    use super::*;

    #[test]
    fn parses_global_reg_domain_before_self_managed_block() {
        let text = "\
global
country BO: DFS-FCC
        (5170 - 5250 @ 80), (24)
phy#3 (self-managed)
country US: DFS-FCC
";
        // The FIRST country line is the global domain.
        assert_eq!(parse_global_reg_domain(text).as_deref(), Some("BO"));
    }

    /// Build a minimal `regulatory.db` image: one country, given rules of
    /// `(start_khz, end_khz, flags)`.
    fn regdb(country: &str, rules: &[(u32, u32, u8)]) -> Vec<u8> {
        let mut db = Vec::new();
        db.extend_from_slice(&REGDB_MAGIC.to_be_bytes());
        db.extend_from_slice(&REGDB_VERSION.to_be_bytes());
        // Country entry + terminator: collection at byte 16 (ptr 4).
        db.extend_from_slice(country.as_bytes());
        db.extend_from_slice(&4u16.to_be_bytes());
        db.extend_from_slice(&[0, 0, 0, 0]);
        // Collection: len 3, n_rules, dfs_region, then rule pointers at 16+4.
        let rules_base = 20 + 2 * rules.len();
        let rules_base = rules_base.div_ceil(4) * 4;
        db.extend_from_slice(&[3, rules.len() as u8, 0, 0]);
        for i in 0..rules.len() {
            db.extend_from_slice(&(((rules_base + 20 * i) >> 2) as u16).to_be_bytes());
        }
        db.resize(rules_base, 0);
        for (start, end, flags) in rules {
            db.extend_from_slice(&[16, *flags]);
            db.extend_from_slice(&3000u16.to_be_bytes());
            db.extend_from_slice(&start.to_be_bytes());
            db.extend_from_slice(&end.to_be_bytes());
            db.extend_from_slice(&80_000u32.to_be_bytes());
            db.extend_from_slice(&[0, 0, 0, 0]);
        }
        db
    }

    #[test]
    fn the_wanted_domain_is_judged_from_its_own_rules() {
        // U-NII-3 open, U-NII-2 radar-only.
        let db = regdb(
            "US",
            &[
                (5_735_000, 5_835_000, 0),
                (5_250_000, 5_330_000, REGDB_FLAG_DFS | REGDB_FLAG_NO_IR),
            ],
        );
        assert_eq!(regdb_permits_channel(&db, "US", 149), Some(true));
        assert_eq!(regdb_permits_channel(&db, "us", 161), Some(true));
        assert_eq!(regdb_permits_channel(&db, "US", 56), Some(false), "DFS");
        assert_eq!(regdb_permits_channel(&db, "US", 36), Some(false), "no rule");
        // A country the image does not carry, or a broken image: unknown.
        assert_eq!(regdb_permits_channel(&db, "BO", 149), None);
        assert_eq!(regdb_permits_channel(&db[..12], "US", 149), None);
        assert_eq!(regdb_permits_channel(b"not a regdb", "US", 149), None);
    }

    #[test]
    fn a_channel_must_fit_inside_the_rule_whole() {
        // 5815-5835 for channel 165 overhangs a rule ending at 5825 MHz.
        let db = regdb("IN", &[(5_725_000, 5_825_000, 0)]);
        assert_eq!(regdb_permits_channel(&db, "IN", 161), Some(true));
        assert_eq!(regdb_permits_channel(&db, "IN", 165), Some(false));
    }
}
