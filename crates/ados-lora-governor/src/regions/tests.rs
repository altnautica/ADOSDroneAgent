use super::*;

#[test]
fn every_row_code_is_unique_and_every_transmitting_row_has_power_rules() {
    let mut codes: Vec<&str> = all().map(|p| p.code).collect();
    let n = codes.len();
    codes.sort_unstable();
    codes.dedup();
    assert_eq!(codes.len(), n);
    assert_eq!(n, 32);
    for p in all() {
        assert!(!p.citation.is_empty(), "{} has no citation", p.code);
        if p.tx {
            assert!(
                p.sub_bands.iter().all(|s| s.power.is_some()),
                "{} transmits without a power rule",
                p.code
            );
        } else {
            assert!(
                p.rules().is_empty(),
                "{} research row declares rules",
                p.code
            );
        }
    }
}

#[test]
fn research_only_rows_are_the_uncited_regions() {
    let rx_only: Vec<&str> = all().filter(|p| !p.tx).map(|p| p.code).collect();
    assert_eq!(
        rx_only,
        ["CN", "NZ", "SG", "HK", "PH", "MX", "AR", "IL", "ZA", "SA"]
    );
}

#[test]
fn india_is_25_mw_erp_and_1_percent_over_an_hour() {
    let p = by_code("IN").unwrap();
    assert_eq!(p.plan_id, "IN865");
    assert_eq!(p.rules(), ["max_power", "duty_cycle"]);
    let sb = p.sub_bands[0];
    assert_eq!(sb.label(), "865.0-867.0");
    assert_eq!(sb.duty.unwrap().budget_us(), 36_000_000);
    assert_eq!(sb.power.unwrap().limit_mw, 25.0);
    assert!(p.apc);
}

#[test]
fn rule_kinds_follow_each_plan() {
    assert_eq!(by_code("US").unwrap().rules(), ["max_power", "dwell_time"]);
    assert_eq!(
        by_code("JP").unwrap().rules(),
        ["max_power", "dwell_time", "lbt"]
    );
    assert_eq!(by_code("AU").unwrap().rules(), ["max_power"]);
    assert_eq!(by_code("EU").unwrap().rules(), ["max_power", "duty_cycle"]);
}

#[test]
fn region_resolution_prefers_lora_region_then_the_country_setting() {
    let r = resolve(Some("eu"), Some("IN")).unwrap();
    assert_eq!((r.plan.code, r.source), ("EU", RegionSource::LoraRegion));
    let r = resolve(None, Some("DE")).unwrap();
    assert_eq!(
        (r.plan.plan_id, r.source),
        ("EU868", RegionSource::NetworkRegulatory)
    );
    let r = resolve(Some(""), Some("LI")).unwrap();
    assert_eq!(r.plan.code, "CH");
    assert_eq!(resolve(Some("AU433"), None).unwrap().plan.plan_id, "AU433");
    assert!(resolve(None, None).is_none());
    assert!(resolve(None, Some("ZZ")).is_none());
    // A research-only country resolves to its row (which then blocks TX).
    assert!(!resolve(None, Some("CN")).unwrap().plan.tx);
}

#[test]
fn sub_band_labels_and_fit() {
    let eu = by_code("EU").unwrap();
    let labels: Vec<String> = eu.sub_bands.iter().map(SubBand::label).collect();
    assert_eq!(
        labels,
        [
            "863.0-865.0",
            "865.0-868.0",
            "868.0-868.6",
            "868.7-869.2",
            "869.4-869.65",
            "869.7-870.0"
        ]
    );
    // A carrier straddling two sub-bands fits neither.
    assert!(eu.sub_band_for(868.0, 125).is_none());
    assert_eq!(eu.sub_band_for(868.1, 125).unwrap().label(), "868.0-868.6");
}

#[test]
fn elrs_domains_follow_the_research_table() {
    assert_eq!(elrs_domains("EU"), (Some("CE_LBT"), Some("EU868")));
    assert_eq!(elrs_domains("IN"), (Some("ISM2G4"), Some("IN866")));
    assert_eq!(elrs_domains("RU"), (None, None));
}
