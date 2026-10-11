use super::*;

fn temp_path(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "ados-seen-jobs-{}-{}/_seen_jobs.json",
        std::process::id(),
        name
    ));
    let _ = std::fs::remove_dir_all(p.parent().unwrap());
    p
}

#[test]
fn mark_and_already_seen_round_trip() {
    let path = temp_path("roundtrip");
    assert!(!already_seen("job-1", &path));
    mark_seen("job-1", &path).unwrap();
    assert!(already_seen("job-1", &path));
    // A different job is still unseen.
    assert!(!already_seen("job-2", &path));
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn missing_file_is_empty_not_an_error() {
    let path = temp_path("missing");
    assert!(load(&path).is_empty());
    assert!(!already_seen("any", &path));
}

#[test]
fn over_cap_drops_oldest_ten_percent() {
    // Build a map over the cap and confirm save drops the oldest ~10%.
    let path = temp_path("cap");
    let mut seen = BTreeMap::new();
    for i in 0..(SEEN_JOBS_MAX + 100) {
        // ts ascending with index so "oldest" is deterministic.
        seen.insert(format!("job-{i:06}"), i as i64);
    }
    save(&seen, &path).unwrap();
    let reloaded = load(&path);
    let total = SEEN_JOBS_MAX + 100;
    let expected_kept = total - total / 10;
    assert_eq!(reloaded.len(), expected_kept);
    // The very oldest (smallest ts) was dropped; a recent one is kept.
    assert!(!reloaded.contains_key("job-000000"));
    assert!(reloaded.contains_key(&format!("job-{:06}", total - 1)));
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn malformed_file_loads_empty() {
    let path = temp_path("malformed");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"not json").unwrap();
    assert!(load(&path).is_empty());
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}
