use super::*;
use crate::archive::parse_archive_bytes;
use std::collections::BTreeMap;
use std::io::Write;
use zip::write::SimpleFileOptions;

use super::tests_support::paths_in;
use crate::backend::RecordingBackend;

fn build_unsigned_archive(manifest_yaml: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        w.start_file("manifest.yaml", opts).unwrap();
        w.write_all(manifest_yaml.as_bytes()).unwrap();
        w.start_file("agent/py/x.py", opts).unwrap();
        w.write_all(b"print('hi')").unwrap();
        w.finish().unwrap();
    }
    buf
}

const SUBPROC_MANIFEST: &str = "id: com.example.thermal\nversion: 1.0.0\nrisk: high\ncompatibility:\n  ados_version: \">=0.1.0,<2.0.0\"\nagent:\n  entrypoint: agent/py/x.py\n  permissions:\n    - hardware.spi\n";

#[test]
fn require_signed_default_is_on_unless_explicitly_falsey() {
    // Serialized env mutation: this is the only test that touches
    // ADOS_PLUGIN_REQUIRE_SIGNED, so it restores the prior value and runs
    // alone within this module's concern.
    let prev = std::env::var("ADOS_PLUGIN_REQUIRE_SIGNED").ok();

    std::env::remove_var("ADOS_PLUGIN_REQUIRE_SIGNED");
    assert!(require_signed_default(), "absent env keeps signing ON");

    for falsey in ["0", "false", "FALSE", "no", "off", " off "] {
        std::env::set_var("ADOS_PLUGIN_REQUIRE_SIGNED", falsey);
        assert!(!require_signed_default(), "{falsey:?} must disable signing");
    }
    for truthy in ["1", "true", "yes", "on", "anything", ""] {
        std::env::set_var("ADOS_PLUGIN_REQUIRE_SIGNED", truthy);
        assert!(require_signed_default(), "{truthy:?} must keep signing ON");
    }

    match prev {
        Some(v) => std::env::set_var("ADOS_PLUGIN_REQUIRE_SIGNED", v),
        None => std::env::remove_var("ADOS_PLUGIN_REQUIRE_SIGNED"),
    }
}

#[test]
fn production_supervisor_requires_signing_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let prev = std::env::var("ADOS_PLUGIN_REQUIRE_SIGNED").ok();
    std::env::remove_var("ADOS_PLUGIN_REQUIRE_SIGNED");
    let sup = PluginSupervisor::production(paths_in(dir.path()), None, "0.48.11");
    assert!(sup.require_signed());
    match prev {
        Some(v) => std::env::set_var("ADOS_PLUGIN_REQUIRE_SIGNED", v),
        None => std::env::remove_var("ADOS_PLUGIN_REQUIRE_SIGNED"),
    }
}

#[test]
fn production_supervisor_rejects_unsigned_archive() {
    // The end-to-end F3 guard at the supervisor layer: a production-built
    // controller refuses an unsigned archive on install_contents.
    let dir = tempfile::tempdir().unwrap();
    let prev = std::env::var("ADOS_PLUGIN_REQUIRE_SIGNED").ok();
    std::env::remove_var("ADOS_PLUGIN_REQUIRE_SIGNED");
    let mut sup = PluginSupervisor::production(paths_in(dir.path()), None, "0.48.11")
        .with_backend(Arc::new(RecordingBackend::default()));
    let contents = parse_archive_bytes(build_unsigned_archive(SUBPROC_MANIFEST)).unwrap();
    let err = sup
        .install_contents(contents, Path::new("/tmp/x.adosplug"))
        .unwrap_err();
    assert!(
        matches!(err, LifecycleError::Signature(_)),
        "an unsigned archive must fail the signature gate: {err}"
    );
    assert!(format!("{err}").contains("unsigned"), "{err}");
    match prev {
        Some(v) => std::env::set_var("ADOS_PLUGIN_REQUIRE_SIGNED", v),
        None => std::env::remove_var("ADOS_PLUGIN_REQUIRE_SIGNED"),
    }
}

#[test]
fn the_install_target_is_always_a_direct_child_of_the_install_dir() {
    let install_dir = Path::new("/var/ados/plugins");
    for id in [
        "../../etc",
        "/etc",
        "com.example/../../etc",
        "a/b",
        "..",
        ".",
        "",
    ] {
        assert!(
            plugin_dir_under(install_dir, id).is_err(),
            "id {id:?} must not resolve to an install target"
        );
    }
    let target = plugin_dir_under(install_dir, "com.example.thermal").unwrap();
    assert_eq!(target, install_dir.join("com.example.thermal"));
    assert_eq!(target.parent(), Some(install_dir));
}

#[test]
fn semver_in_range_boundaries() {
    // 0.48.11 is >= 0.9.0 (minor 48 > 9) and < 1.0.0 -> in range.
    assert!(semver_in_range("0.48.11", ">=0.9.0,<1.0.0").unwrap());
    // 0.8.0 falls below the lower bound.
    assert!(!semver_in_range("0.8.0", ">=0.9.0,<1.0.0").unwrap());
    assert!(semver_in_range("0.48.11", ">=0.1.0").unwrap());
    // The upper bound is exclusive.
    assert!(!semver_in_range("1.0.0", ">=0.1.0,<1.0.0").unwrap());
    // The lower bound is inclusive.
    assert!(semver_in_range("0.9.0", ">=0.9.0").unwrap());
    assert!(!semver_in_range("0.9.0", ">0.9.0").unwrap());
    assert!(semver_in_range("0.9.0", "<=0.9.0").unwrap());
    assert!(semver_in_range("0.9.0", "==0.9.0").unwrap());
    // A bare semver means equality.
    assert!(semver_in_range("0.9.0", "0.9.0").unwrap());
    assert!(!semver_in_range("0.9.1", "0.9.0").unwrap());
    // Pre-release metadata is stripped before comparison.
    assert!(semver_in_range("0.48.11-rc1", ">=0.1.0").unwrap());
    // Unparseable -> error.
    assert!(semver_in_range("x.y.z", ">=0.1.0").is_err());
}

#[test]
fn install_unsigned_subprocess_writes_unit_and_state() {
    let dir = tempfile::tempdir().unwrap();
    let rec = Arc::new(RecordingBackend::default());
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(rec.clone());

    let archive = build_unsigned_archive(SUBPROC_MANIFEST);
    let contents = parse_archive_bytes(archive).unwrap();
    let res = sup
        .install_contents(contents, Path::new("/tmp/thermal.adosplug"))
        .unwrap();
    assert_eq!(res.plugin_id, "com.example.thermal");
    assert_eq!(res.risk, "high");
    assert_eq!(res.permissions_requested, vec!["hardware.spi".to_string()]);

    // The unit was handed to the backend with the most restrictive sandbox,
    // and the attestation landed beside the unpacked tree.
    let unit = rec
        .unit(&unit_name_for("com.example.thermal"))
        .expect("unit installed");
    assert!(unit.contains("PrivateDevices=yes"));
    let attestation = sup.attestation("com.example.thermal").unwrap();
    assert_eq!(attestation.signature, None);
    let paths: Vec<&str> = attestation.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["agent/py/x.py", "manifest.yaml"]);
    assert_eq!(
        attestation.files[1].sha256,
        hex::encode(Sha256::digest(SUBPROC_MANIFEST.as_bytes()))
    );
    assert_eq!(
        sup.installed_manifest_bytes("com.example.thermal").unwrap(),
        SUBPROC_MANIFEST.as_bytes()
    );

    // State persisted with status installed.
    let reloaded = load_state(Some(&dir.path().join("state/plugin-state.json")));
    assert_eq!(reloaded.len(), 1);
    assert_eq!(reloaded[0].status, PluginStatus::Installed);
}

const OTHER_MANIFEST: &str = "id: com.example.other\nversion: 1.0.0\nrisk: low\ncompatibility:\n  ados_version: \">=0.1.0,<2.0.0\"\nagent:\n  entrypoint: agent/py/x.py\n";

/// A long-lived controller must modify what is on disk now, not the list
/// it read at boot. The cloud relay holds one for its whole life while the
/// LAN install path writes the same state file; saving the stale copy
/// erased the other writer's install.
#[test]
fn a_mutation_keeps_an_install_another_writer_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let rec = Arc::new(RecordingBackend::default());
    let mut relay = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(rec.clone());
    relay
        .install_contents(
            parse_archive_bytes(build_unsigned_archive(SUBPROC_MANIFEST)).unwrap(),
            Path::new("/tmp/thermal.adosplug"),
        )
        .unwrap();

    // A second writer (the LAN path) records its own install.
    let mut lan = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(rec.clone());
    lan.discover().unwrap();
    lan.install_contents(
        parse_archive_bytes(build_unsigned_archive(OTHER_MANIFEST)).unwrap(),
        Path::new("/tmp/other.adosplug"),
    )
    .unwrap();

    // The relay's controller, still holding its earlier view, mutates.
    relay.enable("com.example.thermal").unwrap();

    let on_disk = load_state(Some(&dir.path().join("state/plugin-state.json")));
    let ids: BTreeSet<&str> = on_disk.iter().map(|i| i.plugin_id.as_str()).collect();
    assert!(
        ids.contains("com.example.other"),
        "the other writer's install was erased: {ids:?}"
    );
    assert!(ids.contains("com.example.thermal"));
}

/// A reinstall checks the new archive before touching the old install, and
/// stops the running plugin before its files are swapped.
#[test]
fn a_bad_reinstall_keeps_the_working_install_and_a_good_one_stops_the_old_process() {
    let dir = tempfile::tempdir().unwrap();
    let rec = Arc::new(RecordingBackend::default());
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(rec.clone());
    sup.install_contents(
        parse_archive_bytes(build_unsigned_archive(SUBPROC_MANIFEST)).unwrap(),
        Path::new("/tmp/thermal.adosplug"),
    )
    .unwrap();
    sup.enable("com.example.thermal").unwrap();
    let installed_file = dir.path().join("plugins/com.example.thermal/agent/py/x.py");
    assert!(installed_file.exists());

    // v1.1.0 declares a GCS bundle it does not ship: refused.
    let broken = SUBPROC_MANIFEST.replace("version: 1.0.0", "version: 1.1.0")
        + "gcs:\n  entrypoint: gcs/dist/index.js\n";
    let err = sup
        .install_contents(
            parse_archive_bytes(build_unsigned_archive(&broken)).unwrap(),
            Path::new("/tmp/thermal-broken.adosplug"),
        )
        .unwrap_err();
    assert!(format!("{err}").contains("not present"), "{err}");
    assert!(
        installed_file.exists(),
        "a refused archive must leave the working install on disk"
    );
    assert_eq!(
        sup.find_install("com.example.thermal").unwrap().status,
        PluginStatus::Running
    );
    assert!(!dir
        .path()
        .join("plugins/.com.example.thermal.staging")
        .exists());

    // A good v1.1.0 stops the running unit before replacing its files.
    rec.clear_calls();
    let good = SUBPROC_MANIFEST.replace("version: 1.0.0", "version: 1.1.0");
    sup.install_contents(
        parse_archive_bytes(build_unsigned_archive(&good)).unwrap(),
        Path::new("/tmp/thermal-good.adosplug"),
    )
    .unwrap();
    let unit = &unit_name_for("com.example.thermal");
    let calls = rec.calls();
    let stopped = calls
        .iter()
        .position(|c| c == &("stop_disable".to_string(), unit.to_string()));
    let reinstalled = calls
        .iter()
        .position(|c| c == &("install".to_string(), unit.to_string()));
    assert!(
        matches!((stopped, reinstalled), (Some(s), Some(i)) if s < i),
        "the old process must be stopped before the swap: {calls:?}"
    );
    let record = sup.find_install("com.example.thermal").unwrap();
    assert_eq!(record.version, "1.1.0");
    assert_eq!(record.status, PluginStatus::Installed);
}

#[test]
fn enable_then_disable_subprocess_drives_the_backend() {
    let dir = tempfile::tempdir().unwrap();
    let rec = Arc::new(RecordingBackend::default());
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(rec.clone());
    let contents = parse_archive_bytes(build_unsigned_archive(SUBPROC_MANIFEST)).unwrap();
    sup.install_contents(contents, Path::new("/tmp/x.adosplug"))
        .unwrap();

    sup.enable("com.example.thermal").unwrap();
    assert_eq!(
        sup.find_install("com.example.thermal").unwrap().status,
        PluginStatus::Running
    );
    // Idempotent enable: a running plugin is left alone.
    sup.enable("com.example.thermal").unwrap();

    sup.disable("com.example.thermal").unwrap();
    assert_eq!(
        sup.find_install("com.example.thermal").unwrap().status,
        PluginStatus::Disabled
    );
    // Idempotent disable.
    sup.disable("com.example.thermal").unwrap();

    let unit = &unit_name_for("com.example.thermal");
    assert!(rec.called("enable_start", unit));
    assert!(rec.called("stop_disable", unit));
    // The idempotent repeats issued nothing more.
    let count = |verb: &str| rec.calls().iter().filter(|(v, _)| v == verb).count();
    assert_eq!(count("enable_start"), 1);
    assert_eq!(count("stop_disable"), 1);
}

#[test]
fn remove_deletes_unit_dir_and_state() {
    let dir = tempfile::tempdir().unwrap();
    let rec = Arc::new(RecordingBackend::default());
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(rec.clone());
    let contents = parse_archive_bytes(build_unsigned_archive(SUBPROC_MANIFEST)).unwrap();
    sup.install_contents(contents, Path::new("/tmp/x.adosplug"))
        .unwrap();
    sup.enable("com.example.thermal").unwrap();

    sup.remove("com.example.thermal", false).unwrap();
    assert!(sup.find_install("com.example.thermal").is_none());
    assert!(rec.unit(&unit_name_for("com.example.thermal")).is_none());
    assert!(!dir.path().join("plugins/com.example.thermal").exists());
    assert!(load_state(Some(&dir.path().join("state/plugin-state.json"))).is_empty());
}

#[test]
fn grant_rejects_undeclared_permission() {
    let dir = tempfile::tempdir().unwrap();
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(Arc::new(RecordingBackend::default()));
    let contents = parse_archive_bytes(build_unsigned_archive(SUBPROC_MANIFEST)).unwrap();
    sup.install_contents(contents, Path::new("/tmp/x.adosplug"))
        .unwrap();

    sup.grant_permission("com.example.thermal", "hardware.spi")
        .unwrap();
    assert!(state::is_permission_granted(
        sup.find_install("com.example.thermal").unwrap(),
        "hardware.spi"
    ));
    let err = sup
        .grant_permission("com.example.thermal", "mission.write")
        .unwrap_err();
    assert!(matches!(err, LifecycleError::Supervisor(_)));
}

#[test]
fn grant_refuses_a_capability_the_host_cannot_back() {
    // A plugin that declares mission.read can have it recorded as a requested
    // permission, but the active Rust host does not implement mission.read,
    // so the supervisor must refuse the grant rather than let the operator
    // hand out a capability that can only error at call time.
    let dir = tempfile::tempdir().unwrap();
    let ungrantable: BTreeSet<String> = ["mission.read".to_string()].into_iter().collect();
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(Arc::new(RecordingBackend::default()))
        .with_ungrantable_caps(ungrantable);
    let manifest = "id: com.example.mission\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0,<2.0.0\"\nagent:\n  entrypoint: agent/py/x.py\n  permissions:\n    - mission.read\n    - hardware.spi\n";
    let contents = parse_archive_bytes(build_unsigned_archive(manifest)).unwrap();
    sup.install_contents(contents, Path::new("/tmp/mission.adosplug"))
        .unwrap();

    // The dead capability is refused with a clear message.
    let err = sup
        .grant_permission("com.example.mission", "mission.read")
        .unwrap_err();
    assert!(
        format!("{err}").contains("not supported by this agent runtime"),
        "{err}"
    );
    assert!(!state::is_permission_granted(
        sup.find_install("com.example.mission").unwrap(),
        "mission.read"
    ));

    // A still-backed capability the same plugin declares is granted normally.
    sup.grant_permission("com.example.mission", "hardware.spi")
        .unwrap();
    assert!(state::is_permission_granted(
        sup.find_install("com.example.mission").unwrap(),
        "hardware.spi"
    ));
}

#[test]
fn network_access_follows_the_loopback_guard_verdict() {
    let dir = tempfile::tempdir().unwrap();
    let sidecar = dir.path().join("plugin-loopback-guard.json");
    let rec = Arc::new(RecordingBackend::default());
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(rec.clone());
    let manifest = "id: com.example.net\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0,<2.0.0\"\nagent:\n  entrypoint: agent/py/x.py\n  permissions:\n    - network.outbound\n";
    let contents = parse_archive_bytes(build_unsigned_archive(manifest)).unwrap();
    sup.install_contents(contents, Path::new("/tmp/net.adosplug"))
        .unwrap();
    let unit = || rec.unit(&unit_name_for("com.example.net")).unwrap();

    // No guard loaded yet: the grant is refused and nothing is recorded.
    let err = sup
        .grant_permission("com.example.net", "network.outbound")
        .unwrap_err();
    assert!(
        format!("{err}").contains("loopback guard is unavailable"),
        "{err}"
    );
    assert!(!state::is_permission_granted(
        sup.find_install("com.example.net").unwrap(),
        "network.outbound"
    ));

    // Guard active: the grant opens the inet families.
    std::fs::write(&sidecar, r#"{"active":true,"reason":""}"#).unwrap();
    sup.grant_permission("com.example.net", "network.outbound")
        .unwrap();
    assert!(unit().contains("RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK"));
    assert!(!unit().contains("IPAddressDeny="));

    // A later boot where the guard cannot load re-renders the held grant
    // back to the no-grant socket policy.
    std::fs::write(&sidecar, r#"{"active":false,"reason":"nft missing"}"#).unwrap();
    sup.refresh_all_units();
    assert!(unit().contains("RestrictAddressFamilies=AF_UNIX\n"));
    assert!(unit().contains("IPAddressDeny=any"));
}

#[test]
fn grant_allows_all_caps_when_none_are_marked_ungrantable() {
    // With no ungrantable set declared (the default), a cap the host happens
    // not to back is still grantable — the refusal is opt-in via
    // with_ungrantable_caps, so a fully-wired host or a test imposes nothing.
    let dir = tempfile::tempdir().unwrap();
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(Arc::new(RecordingBackend::default()));
    let manifest = "id: com.example.mission\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0,<2.0.0\"\nagent:\n  entrypoint: agent/py/x.py\n  permissions:\n    - mission.read\n";
    let contents = parse_archive_bytes(build_unsigned_archive(manifest)).unwrap();
    sup.install_contents(contents, Path::new("/tmp/mission.adosplug"))
        .unwrap();
    sup.grant_permission("com.example.mission", "mission.read")
        .unwrap();
    assert!(state::is_permission_granted(
        sup.find_install("com.example.mission").unwrap(),
        "mission.read"
    ));
}

#[test]
fn realhost_ungrantable_caps_flow_through_the_grant_gate() {
    // End-to-end: feed the controller the exact set RealHost advertises and
    // confirm a RealHost-dead cap is refused while a RealHost-backed cap the
    // plugin declares is granted. This locks the wiring the daemons use.
    use crate::realhost::RealHost;
    let dir = tempfile::tempdir().unwrap();
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(Arc::new(RecordingBackend::default()))
        .with_ungrantable_caps(RealHost::ungrantable_caps());
    // recording.write and sensor.camera.register are dead on RealHost;
    // mavlink.read is backed.
    let manifest = "id: com.example.rec\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0,<2.0.0\"\nagent:\n  entrypoint: agent/py/x.py\n  permissions:\n    - recording.write\n    - sensor.camera.register\n    - mavlink.read\n";
    let contents = parse_archive_bytes(build_unsigned_archive(manifest)).unwrap();
    sup.install_contents(contents, Path::new("/tmp/rec.adosplug"))
        .unwrap();
    assert!(sup
        .grant_permission("com.example.rec", "recording.write")
        .is_err());
    assert!(sup
        .grant_permission("com.example.rec", "sensor.camera.register")
        .is_err());
    sup.grant_permission("com.example.rec", "mavlink.read")
        .unwrap();
    assert!(state::is_permission_granted(
        sup.find_install("com.example.rec").unwrap(),
        "mavlink.read"
    ));
}

#[test]
fn incompatible_version_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(Arc::new(RecordingBackend::default()));
    let manifest = "id: com.example.old\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.9.0,<0.10.0\"\nagent:\n  entrypoint: agent/py/x.py\n";
    let contents = parse_archive_bytes(build_unsigned_archive(manifest)).unwrap();
    let err = sup
        .install_contents(contents, Path::new("/tmp/x.adosplug"))
        .unwrap_err();
    assert!(matches!(err, LifecycleError::Supervisor(_)), "{err}");
}

#[test]
fn unsupported_board_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let mut sup = PluginSupervisor::new(
        paths_in(dir.path()),
        false,
        Some(BoardId {
            profile: "rpi4b".to_string(),
            soc: "BCM2711".to_string(),
        }),
        "0.48.11",
    )
    .with_backend(Arc::new(RecordingBackend::default()));
    let manifest = "id: com.example.board\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\n  supported_boards: [rock-5c-lite]\nagent:\n  entrypoint: agent/py/x.py\n";
    let contents = parse_archive_bytes(build_unsigned_archive(manifest)).unwrap();
    let err = sup
        .install_contents(contents, Path::new("/tmp/x.adosplug"))
        .unwrap_err();
    let err = format!("{err}");
    assert!(err.starts_with("refused: board_mismatch"), "{err}");
    assert!(err.contains("does not support board rpi4b"), "{err}");
}

#[test]
fn board_identity_reads_the_sidecar_and_stays_lenient_without_one() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        PluginSupervisor::board_identity(&dir.path().join("absent.json")),
        (None, None)
    );
    let sidecar = dir.path().join("board.json");
    std::fs::write(
        &sidecar,
        serde_json::json!({
            "version": 1, "name": "Radxa ROCK 5C Lite (RK3582)", "profile": "rock-5c-lite",
            "model": "x", "tier": 3,
            "ram_mb": 4096, "cpu_cores": 6, "vendor": "radxa", "soc": "rk3582",
            "arch": "aarch64", "hw_video_codecs": [], "npu_tops": 0.0,
            "has_accelerator": false, "local_inference": "none",
            "has_local_inference": false,
        })
        .to_string(),
    )
    .unwrap();
    // The gate keys on the profile stem, never on the display name.
    assert_eq!(
        PluginSupervisor::board_identity(&sidecar),
        (
            Some(BoardId {
                profile: "rock-5c-lite".to_string(),
                soc: "rk3582".to_string()
            }),
            Some(3)
        )
    );
    // An older sidecar without a profile stays lenient; an unknown SoC is empty.
    std::fs::write(
        &sidecar,
        serde_json::json!({
            "version": 1, "name": "x", "model": "x", "tier": 2, "ram_mb": 1,
            "cpu_cores": 1, "vendor": "v", "soc": "unknown", "arch": "a",
            "hw_video_codecs": [], "npu_tops": 0.0, "has_accelerator": false,
            "local_inference": "none", "has_local_inference": false,
        })
        .to_string(),
    )
    .unwrap();
    assert_eq!(PluginSupervisor::board_identity(&sidecar), (None, Some(2)));
    let generic = serde_json::json!({
        "version": 1, "name": "generic-arm64", "profile": "generic-arm64", "model": "x",
        "tier": 2, "ram_mb": 1, "cpu_cores": 1, "vendor": "v", "soc": "unknown",
        "arch": "a", "hw_video_codecs": [], "npu_tops": 0.0, "has_accelerator": false,
        "local_inference": "none", "has_local_inference": false,
    });
    std::fs::write(&sidecar, generic.to_string()).unwrap();
    assert_eq!(
        PluginSupervisor::board_identity(&sidecar).0,
        Some(BoardId {
            profile: "generic-arm64".to_string(),
            soc: String::new()
        })
    );
}

/// Nothing runs an in-process agent half, so it is refused whoever signed
/// it; a first-party signature used to let one install and read enabled
/// while never running.
#[test]
fn inprocess_is_refused_even_from_a_first_party_signer() {
    let dir = tempfile::tempdir().unwrap();
    let key = enrol(&dir.path().join("keys"), FIRST_PARTY);
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(Arc::new(RecordingBackend::default()))
        .with_trusted_keys_dir(dir.path().join("keys"));
    let manifest = "id: com.altnautica.inproc\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  entrypoint: pkg:Class\n  isolation: inprocess\n";
    let err = sup
        .install_contents(
            signed_by(build_unsigned_archive(manifest), FIRST_PARTY, &key),
            Path::new("/tmp/x.adosplug"),
        )
        .unwrap_err();
    assert!(format!("{err}").contains("does not run"), "{err}");
    assert!(sup.installs().is_empty());
}

const INLINE_PANEL: &str = "id: com.example.panel\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\ngcs:\n  entrypoint: gcs/dist/index.js\n  isolation: inline\n";

#[test]
fn inline_gcs_needs_a_verified_first_party_signer() {
    let dir = tempfile::tempdir().unwrap();
    let keys = dir.path().join("keys");
    let first = enrol(&keys, FIRST_PARTY);
    let third = enrol(&keys, "third-party");
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(Arc::new(RecordingBackend::default()))
        .with_trusted_keys_dir(&keys)
        .with_first_party_key(FIRST_PARTY, first.verifying_key());
    let panel = || archive_with(INLINE_PANEL, &[("gcs/dist/index.js", b"x")]);
    for contents in [
        parse_archive_bytes(panel()).unwrap(),
        forged_as(panel(), FIRST_PARTY),
        signed_by(panel(), "third-party", &third),
    ] {
        let err = sup
            .install_contents(contents, Path::new("/tmp/panel.adosplug"))
            .unwrap_err();
        assert!(format!("{err}").contains("inline GCS isolation"), "{err}");
    }
    sup.install_contents(
        signed_by(panel(), FIRST_PARTY, &first),
        Path::new("/tmp/panel.adosplug"),
    )
    .unwrap();
}

/// With signing relaxed an archive still installs, but the signer it
/// declares is recorded (and trusted) only when its signature verifies.
/// Each case is a first install: replacing an install with a build from a
/// different signer is refused on its own.
#[test]
fn the_install_record_names_only_a_verified_signer() {
    let recorded_after = |build: &dyn Fn(&ed25519_dalek::SigningKey) -> ArchiveContents| {
        let dir = tempfile::tempdir().unwrap();
        let key = enrol(&dir.path().join("keys"), FIRST_PARTY);
        let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
            .with_backend(Arc::new(RecordingBackend::default()))
            .with_trusted_keys_dir(dir.path().join("keys"));
        let result = sup
            .install_contents(build(&key), Path::new("/tmp/x.adosplug"))
            .unwrap();
        let state_path = dir.path().join("state/plugin-state.json");
        let recorded = find_install(&load_state(Some(&state_path)), "com.example.thermal")
            .unwrap()
            .signer_id
            .clone();
        (result.signer_id, recorded)
    };

    let forged =
        recorded_after(&|_| forged_as(build_unsigned_archive(SUBPROC_MANIFEST), FIRST_PARTY));
    assert_eq!(forged, (None, None));

    let signed = recorded_after(&|key| {
        signed_by(build_unsigned_archive(SUBPROC_MANIFEST), FIRST_PARTY, key)
    });
    assert_eq!(
        signed,
        (Some(FIRST_PARTY.to_string()), Some(FIRST_PARTY.to_string()))
    );
}

#[test]
fn signing_required_installs_an_enrolled_signature_and_refuses_a_forged_one() {
    let dir = tempfile::tempdir().unwrap();
    let key = enrol(&dir.path().join("keys"), FIRST_PARTY);
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), true, None, "0.48.11")
        .with_backend(Arc::new(RecordingBackend::default()))
        .with_trusted_keys_dir(dir.path().join("keys"));
    let err = sup
        .install_contents(
            forged_as(build_unsigned_archive(SUBPROC_MANIFEST), FIRST_PARTY),
            Path::new("/tmp/x.adosplug"),
        )
        .unwrap_err();
    assert!(
        matches!(&err, LifecycleError::Signature(e) if e.kind == SignatureErrorKind::Invalid),
        "{err}"
    );
    let installed = sup
        .install_contents(
            signed_by(build_unsigned_archive(SUBPROC_MANIFEST), FIRST_PARTY, &key),
            Path::new("/tmp/x.adosplug"),
        )
        .unwrap();
    assert_eq!(installed.signer_id.as_deref(), Some(FIRST_PARTY));
}

#[test]
fn manifest_hash_tamper_is_detected() {
    let dir = tempfile::tempdir().unwrap();
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(Arc::new(RecordingBackend::default()));
    let contents = parse_archive_bytes(build_unsigned_archive(SUBPROC_MANIFEST)).unwrap();
    sup.install_contents(contents, Path::new("/tmp/x.adosplug"))
        .unwrap();

    // Tamper with the on-disk manifest after install.
    let manifest_path = dir.path().join("plugins/com.example.thermal/manifest.yaml");
    let mut tampered = std::fs::read_to_string(&manifest_path).unwrap();
    tampered.push_str("\n# injected\n");
    std::fs::write(&manifest_path, tampered).unwrap();

    // Any lifecycle op that resolves the manifest now fails the hash check.
    let err = sup.enable("com.example.thermal").unwrap_err();
    assert!(format!("{err}").contains("manifest hash mismatch"), "{err}");
}

#[test]
fn discover_filters_tampered_permissions() {
    let dir = tempfile::tempdir().unwrap();
    let state_path = dir.path().join("state/plugin-state.json");
    std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
    // Unpack a manifest declaring only hardware.spi.
    let install_dir = dir.path().join("plugins/com.example.thermal");
    std::fs::create_dir_all(&install_dir).unwrap();
    std::fs::write(install_dir.join("manifest.yaml"), SUBPROC_MANIFEST).unwrap();
    let manifest_hash = hex::encode(Sha256::digest(SUBPROC_MANIFEST.as_bytes()));

    // Write a state file that grants an extra permission the manifest does
    // not declare.
    let mut inst = PluginInstall {
        plugin_id: "com.example.thermal".into(),
        version: "1.0.0".into(),
        source: PluginSource::LocalFile,
        source_uri: None,
        signer_id: None,
        manifest_hash,
        status: PluginStatus::Installed,
        installed_at: now_ms(),
        enabled_at: None,
        failure_reason: None,
        permissions: BTreeMap::new(),
        auto_update: true,
        pinned_version: None,
        last_update_check_at: None,
        last_update_attempt: None,
        reenable_pending: false,
        revocation_pending: false,
        archive_sha256: None,
        model_status: None,
        service_status: None,
    };
    grant_permission(&mut inst, "hardware.spi");
    grant_permission(&mut inst, "mission.write"); // not declared
    save_state(&[inst], Some(&state_path)).unwrap();

    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(Arc::new(RecordingBackend::default()));
    sup.discover().unwrap();
    let install = sup.find_install("com.example.thermal").unwrap();
    assert!(install.permissions.contains_key("hardware.spi"));
    assert!(!install.permissions.contains_key("mission.write"));
}

/// A hung service-manager call must not hold the controller: the call is
/// abandoned at its deadline and reported as a timeout.
#[test]
fn a_hung_service_call_is_abandoned_at_its_deadline() {
    use crate::backend::run_bounded;
    let started = std::time::Instant::now();
    let err = run_bounded(
        "sleep",
        &["30"],
        None,
        std::time::Duration::from_millis(200),
    )
    .unwrap_err();
    assert!(err.0.contains("timed out"), "{}", err.0);
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert!(run_bounded("false", &[], None, std::time::Duration::from_secs(5)).is_err());
    assert_eq!(
        run_bounded("echo", &["hi"], None, std::time::Duration::from_secs(5)).unwrap(),
        "hi\n"
    );
}

/// Declared services follow the plugin's lifecycle on this path exactly as
/// on the LAN path: rendered and started on enable, readiness recorded,
/// stopped on disable, units deleted on remove.
#[test]
fn declared_services_follow_enable_disable_and_remove() {
    let dir = tempfile::tempdir().unwrap();
    let rec = Arc::new(RecordingBackend::default());
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(rec.clone());
    let manifest = SUBPROC_MANIFEST.to_string()
            + "  contributes:\n    services:\n      - name: bridge\n        command: bin/bridge --fast\n";
    sup.install_contents(
        parse_archive_bytes(build_unsigned_archive(&manifest)).unwrap(),
        Path::new("/tmp/thermal.adosplug"),
    )
    .unwrap();
    let svc_unit = &service_unit_name_for("com.example.thermal", "bridge");

    sup.enable("com.example.thermal").unwrap();
    let unit = rec.unit(svc_unit).expect("service unit installed");
    assert!(unit.contains("ExecStart=bin/bridge --fast"));
    assert!(rec.called("enable_start", svc_unit));
    assert_eq!(
        sup.find_install("com.example.thermal")
            .unwrap()
            .service_status,
        Some(serde_json::json!([{"name": "bridge", "ready": true, "reason": null}]))
    );
    assert_eq!(
        sup.service_readiness("com.example.thermal").unwrap(),
        Some(serde_json::json!([{"name": "bridge", "ready": true, "reason": null}]))
    );

    sup.disable("com.example.thermal").unwrap();
    assert!(rec.called("stop_disable", svc_unit));
    assert!(sup
        .find_install("com.example.thermal")
        .unwrap()
        .service_status
        .is_none());

    sup.remove("com.example.thermal", false).unwrap();
    assert!(
        rec.unit(svc_unit).is_none(),
        "the service unit must be deleted"
    );
    assert!(sup.service_readiness("com.example.thermal").is_err());
}

fn archive_with(manifest_yaml: &str, files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        w.start_file("manifest.yaml", opts).unwrap();
        w.write_all(manifest_yaml.as_bytes()).unwrap();
        for (path, bytes) in files {
            w.start_file(*path, opts).unwrap();
            w.write_all(bytes).unwrap();
        }
        w.finish().unwrap();
    }
    buf
}

/// A first-party signer id (on [`crate::signing::FIRST_PARTY_SIGNERS`]).
const FIRST_PARTY: &str = "altnautica-2026-A";

/// A throwaway Ed25519 key enrolled as `signer` in `keys_dir`, the store a
/// supervisor built `.with_trusted_keys_dir(keys_dir)` verifies against.
fn enrol(keys_dir: &Path, signer: &str) -> ed25519_dalek::SigningKey {
    use ed25519_dalek::pkcs8::{spki::der::pem::LineEnding, EncodePublicKey};
    let key = ed25519_dalek::SigningKey::from_bytes(&Sha256::digest(signer.as_bytes()).into());
    std::fs::create_dir_all(keys_dir).unwrap();
    let pem = key
        .verifying_key()
        .to_public_key_pem(LineEnding::LF)
        .unwrap();
    std::fs::write(keys_dir.join(format!("{signer}.pem")), pem).unwrap();
    key
}

/// `archive` with a `SIGNATURE` entry naming `signer` over `signature`.
fn with_signature(archive: Vec<u8>, signer: &str, signature: &str) -> ArchiveContents {
    let mut w = zip::ZipWriter::new_append(std::io::Cursor::new(archive)).unwrap();
    let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    w.start_file("SIGNATURE", opts).unwrap();
    w.write_all(format!("{signer}\n{signature}\n").as_bytes())
        .unwrap();
    parse_archive_bytes(w.finish().unwrap().into_inner()).unwrap()
}

/// `archive` signed by `signer` with `key` over its canonical payload hash.
fn signed_by(archive: Vec<u8>, signer: &str, key: &ed25519_dalek::SigningKey) -> ArchiveContents {
    use base64::Engine;
    use ed25519_dalek::Signer;
    let hash = parse_archive_bytes(archive.clone()).unwrap().payload_hash;
    let signature = base64::engine::general_purpose::STANDARD.encode(key.sign(&hash).to_bytes());
    with_signature(archive, signer, &signature)
}

/// `archive` declaring `signer` with a signature no key verifies.
fn forged_as(archive: Vec<u8>, signer: &str) -> ArchiveContents {
    with_signature(archive, signer, "QUJD")
}

const WORKSTATION_ONLY: &str = "id: com.example.ws\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  entrypoint: agent/py/x.py\n  target_profiles: [workstation, compute]\n";

#[test]
fn an_agent_half_that_does_not_target_this_profile_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(Arc::new(RecordingBackend::default()));
    let err = sup
        .install_contents(
            parse_archive_bytes(build_unsigned_archive(WORKSTATION_ONLY)).unwrap(),
            Path::new("/tmp/ws.adosplug"),
        )
        .unwrap_err();
    assert_eq!(
        format!("{err}"),
        "incompatible: target_profiles excludes drone"
    );
    assert!(sup.installs().is_empty());

    // The same archive installs on a node of a profile it targets.
    let mut compute = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(Arc::new(RecordingBackend::default()))
        .with_profile("compute");
    compute
        .install_contents(
            parse_archive_bytes(build_unsigned_archive(WORKSTATION_ONLY)).unwrap(),
            Path::new("/tmp/ws.adosplug"),
        )
        .unwrap();

    // A GCS-only plugin has no agent half to place and is not gated.
    let gcs = "id: com.example.panel\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\ngcs:\n  entrypoint: gcs/index.js\n";
    sup.install_contents(
        parse_archive_bytes(archive_with(gcs, &[("gcs/index.js", b"x")])).unwrap(),
        Path::new("/tmp/panel.adosplug"),
    )
    .unwrap();
}

const HEAVY: &str = "id: com.altnautica.heavy\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  entrypoint: agent/py/x.py\n  resources:\n    class: heavy\n    max_ram_mb: 16384\n    max_cpu_percent: 800\n    max_pids: 1024\n";

#[test]
fn heavy_resources_are_first_party_only() {
    let dir = tempfile::tempdir().unwrap();
    let keys = dir.path().join("keys");
    let first = enrol(&keys, FIRST_PARTY);
    let third = enrol(&keys, "third-party");
    let rec = Arc::new(RecordingBackend::default());
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(rec.clone())
        .with_trusted_keys_dir(&keys)
        .with_first_party_key(FIRST_PARTY, first.verifying_key());
    for contents in [
        parse_archive_bytes(build_unsigned_archive(HEAVY)).unwrap(),
        signed_by(build_unsigned_archive(HEAVY), "third-party", &third),
        forged_as(build_unsigned_archive(HEAVY), FIRST_PARTY),
    ] {
        let err = sup
            .install_contents(contents, Path::new("/tmp/heavy.adosplug"))
            .unwrap_err();
        assert_eq!(
            format!("{err}"),
            "refused: heavy_resources_first_party_only"
        );
    }
    sup.install_contents(
        signed_by(build_unsigned_archive(HEAVY), FIRST_PARTY, &first),
        Path::new("/tmp/heavy.adosplug"),
    )
    .unwrap();
    let unit = rec.unit(&unit_name_for("com.altnautica.heavy")).unwrap();
    assert!(unit.contains("MemoryMax=16384M") && unit.contains("CPUQuota=800%"));
}

fn thermal_at(version: &str) -> Vec<u8> {
    build_unsigned_archive(
        &SUBPROC_MANIFEST.replace("version: 1.0.0", &format!("version: {version}")),
    )
}

#[test]
fn a_reinstall_from_another_signer_or_unsigned_is_refused_as_a_signer_change() {
    let dir = tempfile::tempdir().unwrap();
    let keys = dir.path().join("keys");
    let alice = enrol(&keys, "alice");
    let mallory = enrol(&keys, "mallory");
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(Arc::new(RecordingBackend::default()))
        .with_trusted_keys_dir(&keys);
    let src = Path::new("/tmp/thermal.adosplug");
    sup.install_contents(signed_by(thermal_at("1.0.0"), "alice", &alice), src)
        .unwrap();
    sup.grant_permission("com.example.thermal", "hardware.spi")
        .unwrap();
    for contents in [
        signed_by(thermal_at("1.1.0"), "mallory", &mallory),
        parse_archive_bytes(thermal_at("1.1.0")).unwrap(),
    ] {
        let err = sup.install_contents(contents, src).unwrap_err();
        assert!(
            format!("{err}").starts_with("refused: signer_change"),
            "{err}"
        );
        let record = sup.find_install("com.example.thermal").unwrap();
        assert_eq!(record.version, "1.0.0");
        assert_eq!(record.signer_id.as_deref(), Some("alice"));
    }
    sup.install_contents(signed_by(thermal_at("1.1.0"), "alice", &alice), src)
        .unwrap();
    assert_eq!(
        sup.find_install("com.example.thermal").unwrap().version,
        "1.1.0"
    );
}

#[test]
fn an_install_whose_unit_name_another_plugin_owns_is_refused() {
    // Plugin A declares a service whose unit name is exactly plugin B's
    // main unit name; B's install must not overwrite A's unit.
    let a = "com.example.a";
    let b = format!("com.example.a-{}", crate::plugin_account::id_hash8(a));
    let svc = crate::plugin_account::id_hash8(&b);
    assert_eq!(service_unit_name_for(a, &svc), unit_name_for(&b));
    let dir = tempfile::tempdir().unwrap();
    let rec = Arc::new(RecordingBackend::default());
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(rec.clone());
    let manifest_a = format!(
            "id: {a}\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  \
             entrypoint: agent/py/x.py\n  contributes:\n    services:\n      - name: \"{svc}\"\n        \
             command: bin/x\n"
        );
    sup.install_contents(
        parse_archive_bytes(build_unsigned_archive(&manifest_a)).unwrap(),
        Path::new("/tmp/a.adosplug"),
    )
    .unwrap();
    let manifest_b = format!(
        "id: {b}\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  \
             entrypoint: agent/py/x.py\n"
    );
    rec.clear_calls();
    let err = sup
        .install_contents(
            parse_archive_bytes(build_unsigned_archive(&manifest_b)).unwrap(),
            Path::new("/tmp/b.adosplug"),
        )
        .unwrap_err();
    assert!(
        format!("{err}").starts_with("refused: unit_name_collision"),
        "{err}"
    );
    assert!(sup.find_install(&b).is_none());
    assert!(!rec.calls().iter().any(|(verb, _)| verb == "install"));
}

#[test]
fn revoking_a_signer_disables_its_installed_plugins_and_blocks_enable() {
    let dir = tempfile::tempdir().unwrap();
    let keys = dir.path().join("keys");
    let alice = enrol(&keys, "alice");
    let revocations = dir.path().join("plugin-revocations.json");
    let rec = Arc::new(RecordingBackend::default());
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(rec.clone())
        .with_trusted_keys_dir(&keys)
        .with_revocations_path(&revocations);
    let id = "com.example.thermal";
    sup.install_contents(
        signed_by(thermal_at("1.0.0"), "alice", &alice),
        Path::new("/tmp/thermal.adosplug"),
    )
    .unwrap();
    sup.enable(id).unwrap();
    // Nothing revoked: nothing changes.
    let none = sup.revocations().unwrap();
    assert_eq!(
        sup.apply_revocations(&none, false).unwrap(),
        Default::default()
    );

    std::fs::write(&revocations, r#"["alice"]"#).unwrap();
    let revs = sup.revocations().unwrap();
    assert_eq!(sup.apply_revocations(&revs, false).unwrap().disabled, [id]);
    let record = sup.find_install(id).unwrap();
    assert_eq!(record.status, PluginStatus::Disabled);
    assert_eq!(
        record.failure_reason.as_deref(),
        Some(SIGNER_REVOKED_REASON)
    );
    assert!(rec.called("stop_disable", &unit_name_for(id)));
    // A second pass has nothing left to do, and the plugin stays off.
    assert_eq!(
        sup.apply_revocations(&revs, false).unwrap(),
        Default::default()
    );
    let err = sup.enable(id).unwrap_err();
    assert!(
        format!("{err}").starts_with("refused: signer_revoked"),
        "{err}"
    );
    // Once the signer is no longer revoked, an enable runs the plugin
    // again and the stale reason is gone from the record.
    std::fs::write(&revocations, "[]").unwrap();
    sup.enable(id).unwrap();
    let record = sup.find_install(id).unwrap();
    assert_eq!(record.status, PluginStatus::Running);
    assert_eq!(record.failure_reason, None);
    // A record that still carries a reason while running (written before
    // enables cleared it) is cleaned by an idempotent enable.
    sup.update_record(id, |i| {
        i.failure_reason = Some(SIGNER_REVOKED_REASON.into())
    })
    .unwrap();
    sup.enable(id).unwrap();
    assert_eq!(sup.find_install(id).unwrap().failure_reason, None);
}

#[test]
fn revocation_deferred_while_armed_then_applied() {
    let dir = tempfile::tempdir().unwrap();
    let keys = dir.path().join("keys");
    let alice = enrol(&keys, "alice");
    let revocations = dir.path().join("plugin-revocations.json");
    let rec = Arc::new(RecordingBackend::default());
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(rec.clone())
        .with_trusted_keys_dir(&keys)
        .with_revocations_path(&revocations);
    let id = "com.example.thermal";
    sup.install_contents(
        signed_by(thermal_at("1.0.0"), "alice", &alice),
        Path::new("/tmp/thermal.adosplug"),
    )
    .unwrap();
    sup.enable(id).unwrap();
    rec.clear_calls();
    std::fs::write(&revocations, r#"{"signers":["alice"],"archives":[]}"#).unwrap();
    let revs = sup.revocations().unwrap();

    // Armed: nothing is stopped, the record says the disable is pending.
    let sweep = sup.apply_revocations(&revs, true).unwrap();
    assert_eq!(sweep.deferred, [id]);
    assert!(sweep.disabled.is_empty());
    assert!(!rec.called("stop_disable", &unit_name_for(id)));
    let record = sup.find_install(id).unwrap();
    assert_eq!(record.status, PluginStatus::Running);
    assert!(record.revocation_pending);
    // A new enable or install of the revoked signer is refused at once.
    assert!(sup
        .install_contents(
            signed_by(thermal_at("1.1.0"), "alice", &alice),
            Path::new("/tmp/thermal.adosplug"),
        )
        .is_err());

    // Disarmed: the next pass applies the disable and clears the flag.
    let sweep = sup.apply_revocations(&revs, false).unwrap();
    assert_eq!(sweep.disabled, [id]);
    assert!(rec.called("stop_disable", &unit_name_for(id)));
    let record = sup.find_install(id).unwrap();
    assert_eq!(record.status, PluginStatus::Disabled);
    assert!(!record.revocation_pending);
}

#[test]
fn malformed_revocations_refuse_signed_enable() {
    let dir = tempfile::tempdir().unwrap();
    let keys = dir.path().join("keys");
    let alice = enrol(&keys, "alice");
    let revocations = dir.path().join("plugin-revocations.json");
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(Arc::new(RecordingBackend::default()))
        .with_trusted_keys_dir(&keys)
        .with_revocations_path(&revocations);
    let id = "com.example.thermal";
    sup.install_contents(
        signed_by(thermal_at("1.0.0"), "alice", &alice),
        Path::new("/tmp/thermal.adosplug"),
    )
    .unwrap();
    std::fs::write(&revocations, b"[\"alice\"").unwrap();
    let err = sup.enable(id).unwrap_err();
    assert!(
        format!("{err}").starts_with("refused: revocation_list_unreadable"),
        "{err}"
    );
    let err = sup
        .install_contents(
            signed_by(thermal_at("1.1.0"), "alice", &alice),
            Path::new("/tmp/thermal.adosplug"),
        )
        .unwrap_err();
    assert!(
        format!("{err}").contains("revocation_list_unreadable"),
        "{err}"
    );
    // A missing file is an empty list: the enable goes through.
    std::fs::remove_file(&revocations).unwrap();
    sup.enable(id).unwrap();
}

#[test]
fn archive_hash_revocation_blocks_install() {
    let dir = tempfile::tempdir().unwrap();
    let revocations = dir.path().join("plugin-revocations.json");
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(Arc::new(RecordingBackend::default()))
        .with_revocations_path(&revocations);
    let bytes = thermal_at("1.0.0");
    let digest = hex::encode(Sha256::digest(&bytes));
    sup.install_contents(
        parse_archive_bytes(bytes.clone()).unwrap(),
        Path::new("/tmp/thermal.adosplug"),
    )
    .unwrap();
    let id = "com.example.thermal";
    assert_eq!(
        sup.find_install(id).unwrap().archive_sha256.as_deref(),
        Some(digest.as_str())
    );
    std::fs::write(
        &revocations,
        format!(
            r#"{{"signers":[],"archives":["{}"]}}"#,
            digest.to_uppercase()
        ),
    )
    .unwrap();
    let err = sup
        .install_contents(
            parse_archive_bytes(bytes).unwrap(),
            Path::new("/tmp/thermal.adosplug"),
        )
        .unwrap_err();
    assert!(
        format!("{err}").contains("refused: archive_revoked"),
        "{err}"
    );
    let err = sup.enable(id).unwrap_err();
    assert!(
        format!("{err}").starts_with("refused: archive_revoked"),
        "{err}"
    );
}

#[test]
fn registry_version_revocation_blocks_install_and_enable() {
    let dir = tempfile::tempdir().unwrap();
    let revocations = dir.path().join("plugin-revocations.json");
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(Arc::new(RecordingBackend::default()))
        .with_revocations_path(&revocations);
    let bytes = thermal_at("1.0.0");
    sup.install_contents(
        parse_archive_bytes(bytes.clone()).unwrap(),
        Path::new("/tmp/thermal.adosplug"),
    )
    .unwrap();
    // The registry revokes a release by id and version, with no digest.
    std::fs::write(
        &revocations,
        br#"{"signer_keys":[],"plugin_versions":["com.example.thermal@1.0.0"],"generated_at":1}"#,
    )
    .unwrap();
    let err = sup
        .install_contents(
            parse_archive_bytes(bytes).unwrap(),
            Path::new("/tmp/thermal.adosplug"),
        )
        .unwrap_err();
    assert!(
        format!("{err}").contains("refused: version_revoked"),
        "{err}"
    );
    let err = sup.enable("com.example.thermal").unwrap_err();
    assert!(
        format!("{err}").starts_with("refused: version_revoked"),
        "{err}"
    );
    // Another release of the same plugin is not caught.
    sup.install_contents(
        parse_archive_bytes(thermal_at("1.0.1")).unwrap(),
        Path::new("/tmp/thermal.adosplug"),
    )
    .unwrap();
}

#[test]
fn forged_first_party_named_key_is_third_party() {
    // A key planted under a first-party NAME verifies, and is recorded as
    // that signer, but buys none of the first-party allowances.
    let dir = tempfile::tempdir().unwrap();
    let keys = dir.path().join("keys");
    let planted = enrol(&keys, FIRST_PARTY);
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(Arc::new(RecordingBackend::default()))
        .with_trusted_keys_dir(&keys);
    let err = sup
        .install_contents(
            signed_by(build_unsigned_archive(HEAVY), FIRST_PARTY, &planted),
            Path::new("/tmp/heavy.adosplug"),
        )
        .unwrap_err();
    assert_eq!(
        format!("{err}"),
        "refused: heavy_resources_first_party_only"
    );
    let panel = archive_with(INLINE_PANEL, &[("gcs/dist/index.js", b"x")]);
    let err = sup
        .install_contents(
            signed_by(panel, FIRST_PARTY, &planted),
            Path::new("/tmp/panel.adosplug"),
        )
        .unwrap_err();
    assert!(format!("{err}").contains("inline GCS isolation"), "{err}");
}

#[test]
fn remove_with_tampered_manifest_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let rec = Arc::new(RecordingBackend::default());
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(rec.clone());
    let id = "com.example.thermal";
    let manifest = SUBPROC_MANIFEST.to_string()
        + "  contributes:\n    services:\n      - name: bridge\n        command: bin/bridge\n";
    sup.install_contents(
        parse_archive_bytes(build_unsigned_archive(&manifest)).unwrap(),
        Path::new("/tmp/thermal.adosplug"),
    )
    .unwrap();
    sup.enable(id).unwrap();
    // A service unit file left in the unit dir, found by name alone.
    let svc_unit = service_unit_name_for(id, "bridge");
    let unit_dir = paths_in(dir.path()).unit_dir;
    std::fs::create_dir_all(&unit_dir).unwrap();
    std::fs::write(unit_dir.join(&svc_unit), "[Unit]\n").unwrap();
    std::fs::write(
        dir.path().join("plugins").join(id).join(MANIFEST_FILENAME),
        "id: tampered\n",
    )
    .unwrap();
    rec.clear_calls();

    let outcome = sup.remove(id, false).unwrap();
    assert!(
        outcome
            .warning
            .as_deref()
            .is_some_and(|w| w.contains("manifest hash mismatch")),
        "{outcome:?}"
    );
    assert!(rec.called("uninstall", &unit_name_for(id)));
    assert!(rec.called("uninstall", &svc_unit));
    assert!(sup.find_install(id).is_none());
    assert!(state::load_state(Some(&paths_in(dir.path()).state_path)).is_empty());
    assert!(!dir.path().join("plugins").join(id).exists());
}

#[test]
fn remove_deletes_service_logs() {
    let dir = tempfile::tempdir().unwrap();
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(Arc::new(RecordingBackend::default()));
    let id = "com.example.thermal";
    let manifest = SUBPROC_MANIFEST.to_string()
            + "  contributes:\n    services:\n      - name: bridge\n        command: bin/bridge\n      - name: relay\n        command: bin/relay\n";
    sup.install_contents(
        parse_archive_bytes(build_unsigned_archive(&manifest)).unwrap(),
        Path::new("/tmp/thermal.adosplug"),
    )
    .unwrap();
    let log_dir = paths_in(dir.path()).log_dir;
    std::fs::create_dir_all(&log_dir).unwrap();
    let logs = [
        crate::systemd::log_path_for(&log_dir, id),
        crate::systemd::service_log_path_for(&log_dir, id, "bridge"),
        crate::systemd::service_log_path_for(&log_dir, id, "relay"),
        // A service an older version declared.
        crate::systemd::service_log_path_for(&log_dir, id, "retired"),
    ];
    for log in &logs {
        std::fs::write(log, "x").unwrap();
    }
    let other = crate::systemd::log_path_for(&log_dir, "com.example.other");
    std::fs::write(&other, "x").unwrap();

    sup.remove(id, false).unwrap();
    for log in &logs {
        assert!(!log.exists(), "{} should be gone", log.display());
    }
    assert!(other.exists(), "another plugin's log is left alone");
}

/// A backend whose unit install fails while `fail` is set.
struct FailingInstall {
    inner: RecordingBackend,
    fail: std::sync::atomic::AtomicBool,
}

impl ServiceBackend for FailingInstall {
    fn install(
        &self,
        name: &str,
        spec: &crate::backend::UnitSpec,
    ) -> Result<bool, SupervisorError> {
        if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(SupervisorError("daemon-reload timed out".to_string()));
        }
        self.inner.install(name, spec)
    }
    fn uninstall(&self, name: &str) -> Result<(), SupervisorError> {
        self.inner.uninstall(name)
    }
    fn enable_start(&self, name: &str) -> Result<(), SupervisorError> {
        self.inner.enable_start(name)
    }
    fn stop_disable(&self, name: &str) -> Result<(), SupervisorError> {
        self.inner.stop_disable(name)
    }
    fn restart(&self, name: &str) -> Result<(), SupervisorError> {
        self.inner.restart(name)
    }
    fn is_active(&self, name: &str) -> Result<bool, SupervisorError> {
        self.inner.is_active(name)
    }
    fn probe(&self, probe: &crate::backend::ProbeSpec) -> Result<(), SupervisorError> {
        self.inner.probe(probe)
    }
    fn enforces_sandbox(&self) -> bool {
        self.inner.enforces_sandbox()
    }
}

#[test]
fn failed_unit_install_rolls_back() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(FailingInstall {
        inner: RecordingBackend::default(),
        fail: std::sync::atomic::AtomicBool::new(false),
    });
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(backend.clone());
    let id = "com.example.thermal";
    sup.install_contents(
        parse_archive_bytes(thermal_at("1.0.0")).unwrap(),
        Path::new("/tmp/thermal.adosplug"),
    )
    .unwrap();
    sup.enable(id).unwrap();
    let old_hash = sup.find_install(id).unwrap().manifest_hash.clone();
    backend.inner.clear_calls();

    backend
        .fail
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(sup
        .install_contents(
            parse_archive_bytes(thermal_at("1.1.0")).unwrap(),
            Path::new("/tmp/thermal.adosplug"),
        )
        .is_err());
    backend
        .fail
        .store(false, std::sync::atomic::Ordering::SeqCst);

    // The old tree is back, the record still matches it, and the old unit
    // was started again.
    let target = dir.path().join("plugins").join(id);
    let on_disk = std::fs::read(target.join(MANIFEST_FILENAME)).unwrap();
    assert_eq!(hex::encode(Sha256::digest(&on_disk)), old_hash);
    assert!(!dir.path().join(format!("plugins/.{id}.retired")).exists());
    let record = sup.find_install(id).unwrap();
    assert_eq!(record.version, "1.0.0");
    assert_eq!(record.manifest_hash, old_hash);
    assert!(backend.inner.called("enable_start", &unit_name_for(id)));
    // Every later lifecycle call still works.
    sup.disable(id).unwrap();
    sup.enable(id).unwrap();
}

#[test]
fn discover_restores_retired() {
    let dir = tempfile::tempdir().unwrap();
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(Arc::new(RecordingBackend::default()));
    let id = "com.example.thermal";
    sup.install_contents(
        parse_archive_bytes(thermal_at("1.0.0")).unwrap(),
        Path::new("/tmp/thermal.adosplug"),
    )
    .unwrap();
    // A crash between the two renames of a swap: no target, a retired tree.
    let target = dir.path().join("plugins").join(id);
    let retired = dir.path().join(format!("plugins/.{id}.retired"));
    std::fs::rename(&target, &retired).unwrap();

    let mut fresh = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(Arc::new(RecordingBackend::default()));
    fresh.discover().unwrap();
    assert!(target.join(MANIFEST_FILENAME).exists());
    assert!(!retired.exists());
    fresh.enable(id).unwrap();
}

#[test]
fn a_backend_without_a_sandbox_installs_first_party_agents_only() {
    let dir = tempfile::tempdir().unwrap();
    let keys = dir.path().join("keys");
    let first = enrol(&keys, "altnautica-2026-B");
    let third = enrol(&keys, "third-party");
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(Arc::new(RecordingBackend::without_sandbox()))
        .with_trusted_keys_dir(&keys)
        .with_first_party_key("altnautica-2026-B", first.verifying_key());
    for contents in [
        parse_archive_bytes(build_unsigned_archive(SUBPROC_MANIFEST)).unwrap(),
        signed_by(
            build_unsigned_archive(SUBPROC_MANIFEST),
            "third-party",
            &third,
        ),
        forged_as(
            build_unsigned_archive(SUBPROC_MANIFEST),
            "altnautica-2026-B",
        ),
    ] {
        let err = sup
            .install_contents(contents, Path::new("/tmp/x.adosplug"))
            .unwrap_err();
        assert_eq!(
            format!("{err}"),
            "refused: sandbox_unavailable_first_party_only"
        );
    }
    assert!(sup.installs().is_empty());
    // A GCS-only plugin runs nothing on the host, so it needs no sandbox.
    let gcs = "id: com.example.panel\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\ngcs:\n  entrypoint: gcs/index.js\n";
    sup.install_contents(
        parse_archive_bytes(archive_with(gcs, &[("gcs/index.js", b"x")])).unwrap(),
        Path::new("/tmp/panel.adosplug"),
    )
    .unwrap();
    // A first-party agent installs, and its network grant does not wait on
    // a loopback guard the backend has no sandbox to pair with.
    let net = SUBPROC_MANIFEST.replace("hardware.spi", "network.outbound");
    sup.install_contents(
        signed_by(build_unsigned_archive(&net), "altnautica-2026-B", &first),
        Path::new("/tmp/x.adosplug"),
    )
    .unwrap();
    sup.grant_permission("com.example.thermal", "network.outbound")
        .unwrap();
}

fn bin_manifest(arch_os: &str) -> String {
    format!(
            "id: com.example.multi\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  entrypoint: \"bin:multi-link\"\n  runtime: rust\n  binaries:\n    multi-link:\n      {arch_os}: bin/{arch_os}/multi-link\n"
        )
}

#[test]
fn a_bin_entrypoint_without_this_hosts_binary_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let rec = Arc::new(RecordingBackend::default());
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(rec.clone());
    let host = host_arch_os();
    let other = if host == "riscv64-linux" {
        "aarch64-linux"
    } else {
        "riscv64-linux"
    };
    let foreign = bin_manifest(other);
    let err = sup
        .install_contents(
            parse_archive_bytes(archive_with(
                &foreign,
                &[(&format!("bin/{other}/multi-link"), b"x")],
            ))
            .unwrap(),
            Path::new("/tmp/multi.adosplug"),
        )
        .unwrap_err();
    assert_eq!(
        format!("{err}"),
        format!("incompatible: no binary for {host}")
    );
    // The host's binary named but not shipped is refused at unpack.
    let native = bin_manifest(&host);
    let err = sup
        .install_contents(
            parse_archive_bytes(archive_with(&native, &[])).unwrap(),
            Path::new("/tmp/multi.adosplug"),
        )
        .unwrap_err();
    assert!(format!("{err}").contains("not present"), "{err}");
    // Shipped: installs and the unit execs the resolved path.
    sup.install_contents(
        parse_archive_bytes(archive_with(
            &native,
            &[(&format!("bin/{host}/multi-link"), b"x")],
        ))
        .unwrap(),
        Path::new("/tmp/multi.adosplug"),
    )
    .unwrap();
    let unit = rec.unit(&unit_name_for("com.example.multi")).unwrap();
    assert!(unit.contains(&format!(
        "/plugins/com.example.multi/bin/{host}/multi-link com.example.multi --socket"
    )));
}

fn payload_manifest(sha256: &str, size: usize) -> String {
    format!(
            "id: com.altnautica.payload\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  entrypoint: \"bin:tool\"\n  runtime: rust\n  binaries:\n    tool:\n      {arch}: bin/tool\n  payloads:\n    - path: bin/tool\n      source: https://github.com/example/releases/download/v1/tool\n      sha256: {sha256}\n      size_bytes: {size}\n    - path: models/other-arch.bin\n      source: https://github.com/example/releases/download/v1/other\n      sha256: {zeros}\n      size_bytes: 4\n      arch_os: riscv64-none\n",
            arch = host_arch_os(),
            zeros = "0".repeat(64),
        )
}

#[test]
fn payloads_land_only_when_their_pinned_digest_matches() {
    let dir = tempfile::tempdir().unwrap();
    let body = b"#!/bin/sh\necho tool\n".to_vec();
    let sha = hex::encode(Sha256::digest(&body));
    let source = Arc::new(crate::download::StaticDownloadSource::default().with(
        "https://github.com/example/releases/download/v1/tool",
        body.clone(),
    ));
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(Arc::new(RecordingBackend::default()))
        .with_download_source(source);

    // A pinned digest the body does not match: refused, nothing installed,
    // nothing left behind.
    let wrong = payload_manifest(&"a".repeat(64), body.len());
    let err = sup
        .install_contents(
            parse_archive_bytes(archive_with(&wrong, &[])).unwrap(),
            Path::new("/tmp/payload.adosplug"),
        )
        .unwrap_err();
    assert!(format!("{err}").contains("sha256 mismatch"), "{err}");
    assert!(sup.installs().is_empty());
    let leftovers: Vec<_> = std::fs::read_dir(dir.path().join("plugins"))
        .map(|d| d.filter_map(Result::ok).map(|e| e.file_name()).collect())
        .unwrap_or_default();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    // The right digest: the payload lands executable in the tree, the
    // payload for another architecture is not fetched, and the
    // attestation lists the payload after the archive files.
    let right = payload_manifest(&sha, body.len());
    sup.install_contents(
        parse_archive_bytes(archive_with(&right, &[])).unwrap(),
        Path::new("/tmp/payload.adosplug"),
    )
    .unwrap();
    let tool = dir.path().join("plugins/com.altnautica.payload/bin/tool");
    assert_eq!(std::fs::read(&tool).unwrap(), body);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&tool).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert!(!dir
        .path()
        .join("plugins/com.altnautica.payload/models/other-arch.bin")
        .exists());
    let attestation = sup.attestation("com.altnautica.payload").unwrap();
    let last = attestation.files.last().unwrap();
    assert_eq!(
        (last.path.as_str(), last.sha256.as_str(), last.payload),
        ("bin/tool", sha.as_str(), true)
    );
    assert!(attestation.files[..attestation.files.len() - 1]
        .iter()
        .all(|f| !f.payload));
}

/// A fetched payload the manifest does not run installs `0755` only when it
/// starts with an ELF or `#!` header, the same rule as archive members.
#[test]
fn payload_modes_follow_the_exec_header_rule() {
    use std::os::unix::fs::PermissionsExt;
    let manifest = PluginManifest::from_yaml_text(&payload_manifest(&"a".repeat(64), 4)).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (payloads, staging) = (dir.path().join("payloads"), dir.path().join("staging"));
    let files: [(&str, &[u8]); 4] = [
        ("bin/aarch64-linux/basalt-ados", b"\x7fELF\x02"),
        ("libexec/run", b"#!/bin/sh\n"),
        ("models/net.bin", b"\x00\x01\x02\x03"),
        ("models/empty", b""),
    ];
    let mut fetched = Vec::new();
    for (path, body) in files {
        let src = payloads.join(path);
        std::fs::create_dir_all(src.parent().unwrap()).unwrap();
        std::fs::write(&src, body).unwrap();
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o666)).unwrap();
        fetched.push((path.to_string(), String::new()));
    }
    place_payloads(&manifest, &payloads, &staging, &fetched).unwrap();
    let mode = |rel: &str| {
        std::fs::metadata(staging.join(rel))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777
    };
    assert_eq!(mode("bin/aarch64-linux/basalt-ados"), 0o755);
    assert_eq!(mode("libexec/run"), 0o755);
    assert_eq!(mode("models/net.bin"), 0o644);
    assert_eq!(mode("models/empty"), 0o644);
}

#[test]
fn services_run_only_on_the_profiles_they_name() {
    let dir = tempfile::tempdir().unwrap();
    let rec = Arc::new(RecordingBackend::default());
    let manifest = SUBPROC_MANIFEST.replace("  permissions:", "  target_profiles: [drone, workstation]\n  permissions:")
            + "  contributes:\n    services:\n      - name: capture\n        command: bin/capture\n        profiles: [drone]\n      - name: node\n        command: bin/node\n        profiles: [workstation]\n";
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(rec.clone())
        .with_profile("workstation");
    sup.install_contents(
        parse_archive_bytes(build_unsigned_archive(&manifest)).unwrap(),
        Path::new("/tmp/x.adosplug"),
    )
    .unwrap();
    sup.enable("com.example.thermal").unwrap();
    assert!(rec
        .unit(&service_unit_name_for("com.example.thermal", "node"))
        .is_some());
    assert!(rec
        .unit(&service_unit_name_for("com.example.thermal", "capture"))
        .is_none());
    assert_eq!(
        sup.service_readiness("com.example.thermal").unwrap(),
        Some(serde_json::json!([{"name": "node", "ready": true, "reason": null}]))
    );
}

#[test]
fn a_shared_topic_another_plugin_owns_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(Arc::new(RecordingBackend::default()));
    let owner = |id: &str| {
        format!(
                "id: {id}\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  entrypoint: agent/py/x.py\n  contributes:\n    shared_topics:\n      - topic: plugin.mapper.pose\n        subscribe_capability: telemetry.read\n"
            )
    };
    sup.install_contents(
        parse_archive_bytes(build_unsigned_archive(&owner("com.first.mapper"))).unwrap(),
        Path::new("/tmp/a.adosplug"),
    )
    .unwrap();
    let err = sup
        .install_contents(
            parse_archive_bytes(build_unsigned_archive(&owner("com.second.mapper"))).unwrap(),
            Path::new("/tmp/b.adosplug"),
        )
        .unwrap_err();
    assert!(
        format!("{err}").contains("already owned by com.first.mapper"),
        "{err}"
    );
    // Reinstalling the owner itself is not a collision.
    sup.install_contents(
        parse_archive_bytes(build_unsigned_archive(&owner("com.first.mapper"))).unwrap(),
        Path::new("/tmp/a.adosplug"),
    )
    .unwrap();
}

#[test]
fn an_http_plugin_gets_its_socket_dir_on_enable_and_loses_it_on_remove() {
    let dir = tempfile::tempdir().unwrap();
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(Arc::new(RecordingBackend::default()));
    let manifest = SUBPROC_MANIFEST.replace("  permissions:", "  http: true\n  permissions:");
    sup.install_contents(
        parse_archive_bytes(build_unsigned_archive(&manifest)).unwrap(),
        Path::new("/tmp/x.adosplug"),
    )
    .unwrap();
    let http_dir = dir.path().join("run/plugin-http/com.example.thermal");
    sup.enable("com.example.thermal").unwrap();
    assert!(http_dir.is_dir());
    sup.remove("com.example.thermal", false).unwrap();
    assert!(!http_dir.exists());
}

/// Account calls recorded instead of touching the account database.
#[derive(Default)]
struct RecordingAccounts {
    calls: parking_lot::Mutex<Vec<String>>,
    handed: parking_lot::Mutex<Vec<PathBuf>>,
}

impl crate::plugin_account::PluginAccounts for RecordingAccounts {
    fn ensure(&self, plugin_id: &str) -> Result<(), SupervisorError> {
        self.calls.lock().push(format!("ensure {plugin_id}"));
        Ok(())
    }
    fn remove(&self, plugin_id: &str) -> Result<(), SupervisorError> {
        self.calls.lock().push(format!("remove {plugin_id}"));
        Ok(())
    }
    fn hand_over(&self, dir: &Path, _plugin_id: &str) -> Result<(), SupervisorError> {
        self.handed.lock().push(dir.to_path_buf());
        Ok(())
    }
}

fn install_thermal(sup: &mut PluginSupervisor) {
    sup.install_contents(
        parse_archive_bytes(build_unsigned_archive(SUBPROC_MANIFEST)).unwrap(),
        Path::new("/tmp/x.adosplug"),
    )
    .unwrap();
}

#[test]
fn install_makes_the_data_dir_the_runner_is_pointed_at_and_remove_honours_keep_data() {
    use std::os::unix::fs::PermissionsExt;
    let id = "com.example.thermal";
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("device-id"), "0a1b2c3d4e5f\n").unwrap();
    let accounts = Arc::new(RecordingAccounts::default());
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(Arc::new(RecordingBackend::default()))
        .with_accounts(accounts.clone());
    install_thermal(&mut sup);
    assert_eq!(*accounts.calls.lock(), ["ensure com.example.thermal"]);
    let handed = &accounts.handed;

    let base = dir.path().join("plugin-data").join(id);
    let drones = base.join("drones");
    let leaf = drones.join("0a1b2c3d4e5f");
    for d in [&base, &drones, &leaf] {
        let mode = std::fs::metadata(d).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{}", d.display());
    }
    // Deepest first: the chain stays the host's until its base is handed over.
    assert_eq!(*handed.lock(), [leaf.clone(), drones, base.clone()]);

    // The dir the runner's environment names is the one that exists.
    sup.enable(id).unwrap();
    let env = std::fs::read_to_string(token_credential_path(id, Some(&dir.path().join("sockets"))))
        .unwrap();
    assert!(
        env.contains(&format!("ADOS_PLUGIN_DATA_DIR={}\n", leaf.display())),
        "{env}"
    );

    std::fs::write(leaf.join("world.sqlite"), b"x").unwrap();
    sup.remove(id, true).unwrap();
    assert!(leaf.join("world.sqlite").exists());
    // Data kept: so is the user that owns it.
    assert!(!accounts
        .calls
        .lock()
        .iter()
        .any(|c| c.starts_with("remove")));
    install_thermal(&mut sup);
    sup.remove(id, false).unwrap();
    assert!(!base.exists());
    assert_eq!(
        accounts.calls.lock().last().map(String::as_str),
        Some("remove com.example.thermal")
    );
}

/// An existing data dir is the plugin account's: a later pass hands it
/// over again but creates and chowns nothing under it, so a link the
/// plugin planted there never aims the host's root writes elsewhere.
#[test]
fn an_existing_data_dir_is_handed_over_again_but_never_entered() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("device-id"), "0a1b2c3d4e5f\n").unwrap();
    let accounts = Arc::new(RecordingAccounts::default());
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(Arc::new(RecordingBackend::default()))
        .with_accounts(accounts.clone());
    let handed = &accounts.handed;
    install_thermal(&mut sup);
    let base = dir.path().join("plugin-data/com.example.thermal");
    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    std::fs::remove_dir_all(base.join("drones")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, base.join("drones")).unwrap();
    handed.lock().clear();

    sup.enable("com.example.thermal").unwrap();
    assert_eq!(*handed.lock(), [base]);
    assert!(std::fs::read_dir(&elsewhere).unwrap().next().is_none());
}

/// A unit binds its socket dir and loads its token credential when it
/// starts, so both exist, with its data dir, before the backend starts it:
/// on a first enable and on a re-enable after the host dropped the socket
/// and token of the disabled plugin. The token is one the host verifies.
#[test]
fn enable_starts_the_unit_only_once_what_it_binds_and_reads_exists() {
    let id = "com.example.thermal";
    let dir = tempfile::tempdir().unwrap();
    let paths = paths_in(dir.path());
    let socket_dir = plugin_socket_dir(&paths.socket_dir, id);
    let env = token_credential_path(id, Some(&paths.socket_dir));
    let data = paths.data_root.join(id);
    let secret = paths.token_secret.clone();
    let at_start = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let rec = {
        let (at_start, socket_dir, env) = (at_start.clone(), socket_dir.clone(), env.clone());
        RecordingBackend::default().on_enable_start(move |name| {
            if name == unit_name_for(id) {
                at_start
                    .lock()
                    .push((socket_dir.is_dir(), env.is_file(), data.is_dir()));
            }
        })
    };
    let mut sup = PluginSupervisor::new(paths, false, None, "0.48.11").with_backend(Arc::new(rec));
    install_thermal(&mut sup);

    sup.enable(id).unwrap();
    sup.disable(id).unwrap();
    std::fs::remove_file(&env).unwrap();
    std::fs::remove_dir_all(&socket_dir).unwrap();
    sup.enable(id).unwrap();
    assert_eq!(*at_start.lock(), [(true, true, true); 2]);

    let body = std::fs::read_to_string(&env).unwrap();
    let token = body
        .lines()
        .find_map(|l| l.strip_prefix("ADOS_PLUGIN_TOKEN="))
        .unwrap();
    let token = ados_protocol::plugin::CapabilityToken::from_token_string(token).unwrap();
    assert_eq!(token.plugin_id, id);
    assert!(shared_issuer(&secret)
        .unwrap()
        .verify(&token, token.issued_at + 1)
        .is_ok());
}

#[test]
fn a_declared_capability_is_found_with_its_owner_and_risk() {
    let dir = tempfile::tempdir().unwrap();
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(Arc::new(RecordingBackend::default()));
    let manifest = "id: com.example.mapper\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  entrypoint: agent/py/x.py\n  declared_capabilities:\n    - id: plugin.mapper.world.read\n      risk: low\n      description: Read the world model\n";
    sup.install_contents(
        parse_archive_bytes(build_unsigned_archive(manifest)).unwrap(),
        Path::new("/tmp/m.adosplug"),
    )
    .unwrap();
    let (owner, cap) = sup.declared_capability("plugin.mapper.world.read").unwrap();
    assert_eq!(owner, "com.example.mapper");
    assert_eq!(cap.risk, "low");
    assert!(sup.declared_capability("plugin.mapper.other").is_none());
}
