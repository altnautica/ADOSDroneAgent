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
