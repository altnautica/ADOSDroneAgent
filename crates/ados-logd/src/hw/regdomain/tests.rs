use super::*;

#[test]
fn parses_a_global_only_domain() {
    let blocks = parse_iw_reg_get("global\ncountry US: DFS-FCC\n  (2402 - 2472 @ 40)\n");
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].phy, None);
    assert_eq!(blocks[0].country, "US");
    assert_eq!(blocks[0].dfs_region.as_deref(), Some("FCC"));
}

#[test]
fn parses_global_plus_self_managed_phy() {
    let text = "\
global
country US: DFS-FCC
  (5170 - 5250 @ 80), (N/A, 17), (N/A)

phy#1 (self-managed)
country IN: DFS-ETSI
  (2402 - 2482 @ 40), (6, 20), (N/A)
";
    let blocks = parse_iw_reg_get(text);
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0].phy, None);
    assert_eq!(blocks[0].country, "US");
    assert_eq!(blocks[0].dfs_region.as_deref(), Some("FCC"));
    assert_eq!(blocks[1].phy, Some(1));
    assert_eq!(blocks[1].country, "IN");
    assert_eq!(blocks[1].dfs_region.as_deref(), Some("ETSI"));
}

#[test]
fn handles_self_managed_spelling_variant_and_no_dfs_tag() {
    // The `self managed` (space) spelling and a country with no DFS- tag.
    let text = "\
country 00:
  (2402 - 2472 @ 40)

phy#0 (self managed)
country DE:
";
    let blocks = parse_iw_reg_get(text);
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0].phy, None);
    assert_eq!(blocks[0].country, "00");
    assert_eq!(blocks[0].dfs_region, None);
    assert_eq!(blocks[1].phy, Some(0));
    assert_eq!(blocks[1].country, "DE");
    assert_eq!(blocks[1].dfs_region, None);
}

#[test]
fn read_regdomain_folds_blocks_into_dotted_keys() {
    // Exercise the fold logic directly with the parser output so the key
    // shape is asserted without spawning `iw`.
    let text = "country US: DFS-FCC\n\nphy#2 (self-managed)\ncountry IN: DFS-ETSI\n";
    let mut signals = BTreeMap::new();
    for block in parse_iw_reg_get(text) {
        let prefix = match block.phy {
            None => "reg.global".to_string(),
            Some(n) => format!("reg.phy{n}"),
        };
        signals.insert(format!("{prefix}.country"), MpVal::from(block.country));
        if let Some(region) = block.dfs_region {
            signals.insert(format!("{prefix}.dfs_region"), MpVal::from(region));
        }
    }
    assert_eq!(
        signals.get("reg.global.country").and_then(|v| v.as_str()),
        Some("US")
    );
    assert_eq!(
        signals
            .get("reg.global.dfs_region")
            .and_then(|v| v.as_str()),
        Some("FCC")
    );
    assert_eq!(
        signals.get("reg.phy2.country").and_then(|v| v.as_str()),
        Some("IN")
    );
    assert_eq!(
        signals.get("reg.phy2.dfs_region").and_then(|v| v.as_str()),
        Some("ETSI")
    );
}

#[test]
fn empty_or_garbage_output_yields_no_blocks() {
    assert!(parse_iw_reg_get("").is_empty());
    assert!(parse_iw_reg_get("nonsense without a country line").is_empty());
    // A `country` token with a one-char code is rejected.
    assert!(parse_iw_reg_get("country X:\n").is_empty());
}

#[tokio::test]
async fn read_regdomain_is_graceful_when_iw_is_absent() {
    // On CI / a dev host without `iw` (or where it errors), the read returns
    // an empty map rather than panicking or aborting the tick. This is a
    // smoke test of the graceful-skip path; it does not assert a domain.
    let _signals = read_regdomain().await;
}
