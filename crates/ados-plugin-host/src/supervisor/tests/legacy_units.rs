//! Units left under an install's legacy, hash-free name.

use super::*;

#[test]
fn a_unit_left_under_the_legacy_name_is_removed_and_cleared() {
    let dir = tempfile::tempdir().unwrap();
    let rec = Arc::new(RecordingBackend::default());
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(rec.clone());
    let contents = parse_archive_bytes(build_unsigned_archive(SUBPROC_MANIFEST)).unwrap();
    sup.install_contents(contents, Path::new("/tmp/thermal.adosplug"))
        .unwrap();
    let legacy = "ados-plugin-com-example-thermal.service";
    let unit_dir = dir.path().join("units");
    std::fs::create_dir_all(&unit_dir).unwrap();
    std::fs::write(unit_dir.join(legacy), b"").unwrap();
    rec.clear_calls();

    sup.refresh_all_units();

    assert!(rec.called("stop_disable", legacy));
    assert!(rec.called("uninstall", legacy));
    assert!(rec.called("reset-failed", legacy));
    // The current unit is refreshed, never removed.
    assert!(!rec.called("uninstall", &unit_name_for("com.example.thermal")));
}
