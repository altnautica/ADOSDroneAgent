use super::*;

#[test]
fn query_params_split_repeated_keys_and_decode() {
    let p =
        QueryParams::parse("kind=logs&source=api&source=ados-video&text=hello%20world&limit=50");
    assert_eq!(p.get("kind"), Some("logs"));
    assert_eq!(p.get("text"), Some("hello world"));
    assert_eq!(p.get("limit"), Some("50"));
    assert_eq!(
        p.get_all("source"),
        vec!["api".to_string(), "ados-video".to_string()]
    );
}

#[test]
fn empty_query_is_an_empty_map() {
    let p = QueryParams::parse("");
    assert_eq!(p.get("kind"), None);
    assert!(p.get_all("source").is_empty());
}

#[test]
fn relative_time_bounds_resolve_against_now() {
    let now = 10_000_000_000; // 10_000 seconds in micros
    assert_eq!(
        parse_time(Some("-5s"), now, "from").unwrap(),
        Some(now - 5_000_000)
    );
    assert_eq!(
        parse_time(Some("-2m"), now, "from").unwrap(),
        Some(now - 120_000_000)
    );
    assert_eq!(
        parse_time(Some("-1h"), now, "from").unwrap(),
        Some(now - 3_600_000_000)
    );
    assert_eq!(
        parse_time(Some("-1d"), now, "from").unwrap(),
        Some(now - 86_400_000_000)
    );
    assert_eq!(
        parse_time(Some("-500ms"), now, "from").unwrap(),
        Some(now - 500_000)
    );
}

#[test]
fn absolute_epoch_and_iso_time_bounds() {
    assert_eq!(
        parse_time(Some("1700000000000000"), 0, "from").unwrap(),
        Some(1_700_000_000_000_000)
    );
    // 1970-01-01T00:00:01Z == 1_000_000 micros.
    assert_eq!(
        parse_time(Some("1970-01-01T00:00:01Z"), 0, "from").unwrap(),
        Some(1_000_000)
    );
    // A space separator and no zone are accepted.
    assert_eq!(
        parse_time(Some("1970-01-01 00:01:00"), 0, "from").unwrap(),
        Some(60_000_000)
    );
}

#[test]
fn malformed_time_is_rejected() {
    assert!(parse_time(Some("-5x"), 0, "from").is_err());
    assert!(parse_time(Some("not a time"), 0, "from").is_err());
    assert_eq!(parse_time(None, 0, "from").unwrap(), None);
    assert_eq!(parse_time(Some(""), 0, "from").unwrap(), None);
}

#[test]
fn level_parses_names_and_ordinals() {
    assert_eq!(parse_level("warn").unwrap(), Level::Warn);
    assert_eq!(parse_level("ERROR").unwrap(), Level::Error);
    assert_eq!(parse_level("3").unwrap(), Level::Warn);
    assert_eq!(parse_level("0").unwrap(), Level::Trace);
    assert!(parse_level("9").is_err());
    assert!(parse_level("loud").is_err());
}

#[test]
fn since_aliases_a_relative_from_when_from_is_absent() {
    let now = 5_000_000_000;
    let p = QueryParams::parse("since=-1m");
    let f = QueryFilters::parse(&p, now).unwrap();
    assert_eq!(f.from_us, Some(now - 60_000_000));
}

#[test]
fn limit_is_clamped_and_defaults() {
    let now = 0;
    assert_eq!(
        QueryFilters::parse(&QueryParams::parse(""), now)
            .unwrap()
            .limit,
        DEFAULT_LIMIT
    );
    assert_eq!(
        QueryFilters::parse(&QueryParams::parse("limit=999999"), now)
            .unwrap()
            .limit,
        MAX_LIMIT
    );
    assert_eq!(
        QueryFilters::parse(&QueryParams::parse("limit=0"), now)
            .unwrap()
            .limit,
        1
    );
}

#[test]
fn fingerprint_ignores_source_list_order() {
    let a = QueryFilters::parse(&QueryParams::parse("source=a&source=b"), 0).unwrap();
    let b = QueryFilters::parse(&QueryParams::parse("source=b&source=a"), 0).unwrap();
    assert_eq!(a.fingerprint(), b.fingerprint());
}

#[test]
fn fingerprint_changes_with_table_and_level() {
    let logs = QueryFilters::parse(&QueryParams::parse("kind=logs&level=warn"), 0).unwrap();
    let events = QueryFilters::parse(&QueryParams::parse("kind=events&level=warn"), 0).unwrap();
    let logs_info = QueryFilters::parse(&QueryParams::parse("kind=logs&level=info"), 0).unwrap();
    assert_ne!(logs.fingerprint(), events.fingerprint());
    assert_ne!(logs.fingerprint(), logs_info.fingerprint());
}

#[test]
fn reversed_range_is_rejected() {
    let p = QueryParams::parse("from=200&to=100");
    assert!(QueryFilters::parse(&p, 0).is_err());
}

#[test]
fn unsynced_flag_parses_and_changes_the_fingerprint() {
    let plain = QueryFilters::parse(&QueryParams::parse("kind=logs"), 0).unwrap();
    assert!(!plain.unsynced_only);
    let one = QueryFilters::parse(&QueryParams::parse("kind=logs&unsynced=1"), 0).unwrap();
    assert!(one.unsynced_only);
    let truthy = QueryFilters::parse(&QueryParams::parse("kind=logs&unsynced=true"), 0).unwrap();
    assert!(truthy.unsynced_only);
    // A non-true value leaves the flag off.
    let off = QueryFilters::parse(&QueryParams::parse("kind=logs&unsynced=0"), 0).unwrap();
    assert!(!off.unsynced_only);
    // The flag folds into the fingerprint so a cursor cannot cross between an
    // unsynced-only export and a full one.
    assert_ne!(plain.fingerprint(), one.fingerprint());
}
