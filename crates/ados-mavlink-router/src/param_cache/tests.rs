use super::*;

#[test]
fn set_get_and_count() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = ParamCache::new(dir.path().join("params.json"));
    assert_eq!(c.count(), 0);
    c.set("WPNAV_SPEED", 500.0, 9);
    c.set("ATC_RAT_RLL_P", 0.135, 9);
    assert_eq!(c.count(), 2);
    assert_eq!(c.get("WPNAV_SPEED"), Some(500.0));
    assert_eq!(c.get("MISSING"), None);
}

#[test]
fn generation_starts_at_zero_and_counts_only_real_changes() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = ParamCache::new(dir.path().join("params.json"));
    assert_eq!(c.generation(), 0, "a fresh cache has seen no change");

    // A new name is a change.
    assert!(c.set("WPNAV_SPEED", 500.0, 9));
    assert_eq!(c.generation(), 1);

    // The FC re-announcing the value already cached is NOT a change. This is
    // the case that matters: a PARAM_REQUEST_LIST sweep re-sends every
    // parameter, and bumping the counter for each would make every consumer
    // refetch the whole map on every sweep.
    assert!(!c.set("WPNAV_SPEED", 500.0, 9));
    assert!(!c.set("WPNAV_SPEED", 500.0, 9));
    assert_eq!(c.generation(), 1, "an identical write must not bump");

    // A different value is a change.
    assert!(c.set("WPNAV_SPEED", 750.0, 9));
    assert_eq!(c.generation(), 2);

    // So is the same value under a different declared type: the on-disk cache
    // records `param_type`, so a consumer that refetched would see a
    // different document.
    assert!(c.set("WPNAV_SPEED", 750.0, 6));
    assert_eq!(c.generation(), 3);

    // A second name is its own change.
    assert!(c.set("ATC_RAT_RLL_P", 0.135, 9));
    assert_eq!(c.generation(), 4);
}

#[test]
fn generation_is_not_persisted_and_load_does_not_bump_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("params.json");
    {
        let mut c = ParamCache::new(&path);
        c.set("FOO", 1.5, 9);
        c.set("BAR", -2.0, 6);
        assert_eq!(c.generation(), 2);
        c.save().unwrap();
    }
    // A restart reads the values back but starts the counter at 0, which
    // reads as a mismatch to every consumer and costs one refetch — the
    // correct behaviour, since a restart may have lost cache entries.
    let mut reloaded = ParamCache::new(&path);
    reloaded.load().unwrap();
    assert_eq!(reloaded.count(), 2);
    assert_eq!(reloaded.generation(), 0);
}

#[test]
fn a_nan_value_does_not_bump_the_generation_on_every_write() {
    // `NaN != NaN` under PartialEq, so a naive comparison would report a
    // change on every re-announcement of a NaN parameter and hold every
    // consumer in a permanent refetch loop.
    let dir = tempfile::tempdir().unwrap();
    let mut c = ParamCache::new(dir.path().join("params.json"));
    assert!(c.set("BROKEN", f64::NAN, 9));
    assert_eq!(c.generation(), 1);
    assert!(!c.set("BROKEN", f64::NAN, 9));
    assert_eq!(c.generation(), 1);
}

#[test]
fn save_then_load_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("params.json");
    {
        let mut c = ParamCache::new(&path);
        c.set("FOO", 1.5, 9);
        c.set("BAR", -2.0, 6);
        c.save().unwrap();
    }
    let mut c2 = ParamCache::new(&path);
    c2.load().unwrap();
    assert_eq!(c2.count(), 2);
    assert_eq!(c2.get("FOO"), Some(1.5));
    assert_eq!(c2.get("BAR"), Some(-2.0));
}

#[test]
fn load_missing_file_is_empty_not_error() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = ParamCache::new(dir.path().join("does-not-exist.json"));
    c.load().unwrap();
    assert_eq!(c.count(), 0);
}

#[test]
fn load_corrupt_file_starts_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("params.json");
    std::fs::write(&path, b"{ this is not json").unwrap();
    let mut c = ParamCache::new(&path);
    c.load().unwrap();
    assert_eq!(c.count(), 0);
}

#[test]
fn save_creates_parent_dir() {
    let dir = tempfile::tempdir().unwrap();
    let nested = dir.path().join("a/b/c/params.json");
    let mut c = ParamCache::new(&nested);
    c.set("X", 1.0, 9);
    c.save().unwrap();
    assert!(nested.exists());
}

#[test]
fn serialize_then_write_atomic_matches_save_and_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested/params.json");
    let mut c = ParamCache::new(&path);
    c.set("FOO", 1.5, 9);
    c.set("BAR", -2.0, 6);

    // The off-reactor split (serialize under the lock, write later) must
    // produce a file equivalent to the synchronous save() path.
    let body = c.serialize().unwrap();
    write_atomic(&path, &body).unwrap();
    assert!(path.exists(), "write_atomic creates the parent dir");

    let mut reloaded = ParamCache::new(&path);
    reloaded.load().unwrap();
    assert_eq!(reloaded.count(), 2);
    assert_eq!(reloaded.get("FOO"), Some(1.5));
    assert_eq!(reloaded.get("BAR"), Some(-2.0));
}

#[test]
fn path_accessor_returns_backing_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("params.json");
    let c = ParamCache::new(&path);
    assert_eq!(c.path(), path.as_path());
}
