use super::*;
use std::fs;
use std::path::Path;

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

#[test]
fn parses_pss_from_a_rollup_body() {
    let body = "\
Rss:                4096 kB
Pss:                1234 kB
Pss_Anon:            900 kB
Shared_Clean:        100 kB
";
    assert_eq!(parse_pss_kib(body), Some(1234));
}

#[test]
fn pss_is_none_when_the_line_is_absent() {
    let body = "Rss:   4096 kB\nShared_Clean:   100 kB\n";
    assert_eq!(parse_pss_kib(body), None);
    // An empty body has no Pss line either.
    assert_eq!(parse_pss_kib(""), None);
}

#[test]
fn pss_is_none_for_an_unparseable_value() {
    // A malformed value (no number) yields None rather than a wrong reading.
    assert_eq!(parse_pss_kib("Pss:   notanumber kB\n"), None);
}

#[test]
fn samples_present_services_and_drops_absent_ones() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "101/smaps_rollup", "Pss:   500 kB\n");
    write(root, "202/smaps_rollup", "Rss: 4096 kB\nPss:   2048 kB\n");
    // 303 has a rollup with no Pss line; it must be dropped.
    write(root, "303/smaps_rollup", "Rss: 100 kB\n");
    let units = vec![
        ("ados-supervisor".to_string(), 101u32),
        ("ados-video".to_string(), 202u32),
        ("ados-radio".to_string(), 303u32),
        // 404 has no rollup file at all; it must be dropped.
        ("ados-cloud".to_string(), 404u32),
    ];
    let sampled = sample_service_pss_in(root, &units);
    assert_eq!(
        sampled,
        vec![
            ServiceMemory {
                name: "ados-supervisor".to_string(),
                pss_kib: 500,
            },
            ServiceMemory {
                name: "ados-video".to_string(),
                pss_kib: 2048,
            },
        ]
    );
}

#[test]
fn empty_units_yields_no_samples() {
    let dir = tempfile::tempdir().unwrap();
    assert!(sample_service_pss_in(dir.path(), &[]).is_empty());
}

#[tokio::test]
async fn resolve_is_graceful_when_systemctl_is_absent() {
    // On CI / a dev host without systemd (or off Linux), the resolver returns
    // a vec without panicking or aborting. This is a smoke test of the
    // best-effort path; it does not assert any specific units.
    let _pids = resolve_ados_unit_pids().await;
}
