use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use ados_plugin_host::backend::RecordingBackend;
use ados_plugin_host::download::StaticDownloadSource;
use ados_plugin_host::Paths;
use sha2::{Digest, Sha256};

use super::*;

const URL: &str = "https://github.com/altnautica/ADOSExtensions/releases/download/x/nav.adosplug";

fn paths_in(dir: &Path) -> Paths {
    Paths {
        install_dir: dir.join("plugins"),
        unit_dir: dir.join("units"),
        state_path: dir.join("state/plugin-state.json"),
        log_dir: dir.join("logs"),
        control_dir: dir.join("plugin-host"),
        loopback_guard_state: dir.join("plugin-loopback-guard.json"),
        socket_dir: dir.join("sockets"),
        token_secret: dir.join("secrets/plugin-token-secret"),
        runner: dir.join("bin/ados-plugin-runner"),
        run_dir: dir.join("run"),
        data_root: dir.join("plugin-data"),
        device_id_file: dir.join("device-id"),
    }
}

/// An unsigned single-file archive; the test controller does not enforce
/// signing.
fn archive(id: &str) -> Vec<u8> {
    let manifest = format!(
        "id: {id}\nversion: 1.0.0\nrisk: low\ncompatibility:\n  ados_version: \">=0.1.0,<99.0.0\"\n\
agent:\n  entrypoint: agent/py/x.py\n  target_profiles: [drone, workstation]\n  permissions:\n    - hardware.spi\n"
    );
    let mut buf = Vec::new();
    {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        w.start_file("manifest.yaml", opts).unwrap();
        w.write_all(manifest.as_bytes()).unwrap();
        w.start_file("agent/py/x.py", opts).unwrap();
        w.write_all(b"print('hi')").unwrap();
        w.finish().unwrap();
    }
    buf
}

fn supervisor(dir: &Path) -> PluginSupervisor {
    PluginSupervisor::new(paths_in(dir), false, None, "0.100.0")
        .with_backend(Arc::new(RecordingBackend::default()))
        .with_profile("drone")
}

/// Install `id` from a local archive on `sup`, granted, and switched on when
/// `on` is set.
fn install_local(sup: &mut PluginSupervisor, dir: &Path, id: &str, on: bool) {
    let staged = dir.join(format!("{id}.adosplug"));
    std::fs::write(&staged, archive(id)).unwrap();
    sup.install_archive(&staged).unwrap();
    sup.grant_permission(id, "hardware.spi").unwrap();
    if on {
        sup.enable(id).unwrap();
    }
}

/// A drone with vision-nav installed and running.
fn with_vision_nav(dir: &Path) -> PluginSupervisor {
    let mut sup = supervisor(dir);
    install_local(&mut sup, dir, VISION_NAV_ID, true);
    sup
}

fn catalog(url: &str, sha: &str) -> String {
    serde_json::json!({
        "schema_version": 1,
        "plugins": [{ "id": NAVIGATION_ID, "version": "1.0.0", "download_url": url, "archive_sha256": sha }],
    })
    .to_string()
}

/// A catalog and download source publishing Navigation only.
fn navigation_only() -> (String, StaticDownloadSource) {
    let body = archive(NAVIGATION_ID);
    let sha = hex::encode(Sha256::digest(&body));
    (
        catalog(URL, &sha),
        StaticDownloadSource::default().with(URL, body),
    )
}

fn is_on(sup: &PluginSupervisor, id: &str) -> bool {
    matches!(
        sup.find_install(id).map(|i| i.status),
        Some(PluginStatus::Enabled | PluginStatus::Running)
    )
}

/// [`retire_all`]'s retirements alone.
fn retirements(
    sup: &mut PluginSupervisor,
    catalog: &str,
    source: &dyn DownloadSource,
    guard_wait: Duration,
    retry_delay: Duration,
) -> Vec<Retirement> {
    retire_all(sup, catalog, source, guard_wait, retry_delay).retirements
}

/// A workstation with world-engine installed, switched on when `on` is set,
/// and a catalog publishing Reconstruct.
fn workstation_with_world_engine(
    dir: &Path,
    on: bool,
) -> (PluginSupervisor, String, StaticDownloadSource) {
    let mut sup = supervisor(dir).with_profile("workstation");
    install_local(&mut sup, dir, WORLD_ENGINE_ID, on);
    let url = "https://github.com/altnautica/ADOSExtensions/releases/download/x/rec.adosplug";
    let body = archive(RECONSTRUCT_ID);
    let catalog = serde_json::json!({
        "schema_version": 1,
        "plugins": [{ "id": RECONSTRUCT_ID, "version": "1.0.0", "download_url": url,
                      "archive_sha256": hex::encode(Sha256::digest(&body)) }],
    })
    .to_string();
    (
        sup,
        catalog,
        StaticDownloadSource::default().with(url, body),
    )
}

#[test]
fn the_default_on_reconstruct_install_keeps_a_successor_left_off_off() {
    let dir = tempfile::tempdir().unwrap();
    let (mut sup, catalog, source) = workstation_with_world_engine(dir.path(), false);
    let done = retire_then_install(
        &mut sup,
        &catalog,
        &source,
        Duration::ZERO,
        Duration::ZERO,
        true,
    );
    assert!(
        matches!(
            &done.retired[..],
            [Retirement::Replaced {
                retired: WORLD_ENGINE_ID,
                ..
            }]
        ),
        "{:?}",
        done.retired
    );
    assert!(
        done.reconstruct.as_ref().is_some_and(|r| r.is_ok()),
        "{:?}",
        done.reconstruct
    );
    sup.discover().unwrap();
    assert!(sup.find_install(RECONSTRUCT_ID).is_some());
    assert!(
        !is_on(&sup, RECONSTRUCT_ID),
        "world-engine was off, so its successor stays off"
    );
}

#[test]
fn the_default_on_reconstruct_install_switches_on_a_successor_of_a_running_extension() {
    let dir = tempfile::tempdir().unwrap();
    let (mut sup, catalog, source) = workstation_with_world_engine(dir.path(), true);
    let left = retire_all(&mut sup, &catalog, &source, Duration::ZERO, Duration::ZERO).left_off;
    assert!(left.is_empty(), "{left:?}");
    sup.discover().unwrap();
    assert!(is_on(&sup, RECONSTRUCT_ID));
}

#[test]
fn each_retired_extension_names_its_successors_by_profile() {
    assert_eq!(successors(VISION_NAV_ID, "drone"), [NAVIGATION_ID]);
    assert_eq!(successors(VISION_NAV_ID, "visor"), [NAVIGATION_ID]);
    assert_eq!(
        successors(WORLD_ENGINE_ID, "drone"),
        [NAVIGATION_ID, CAPTURE_ID]
    );
    assert_eq!(
        successors(WORLD_ENGINE_ID, "visor"),
        [NAVIGATION_ID, CAPTURE_ID]
    );
    assert_eq!(successors(WORLD_ENGINE_ID, "ground_station"), [CAPTURE_ID]);
    assert_eq!(successors(WORLD_ENGINE_ID, "workstation"), [RECONSTRUCT_ID]);
    assert_eq!(successors(WORLD_ENGINE_ID, "compute"), [RECONSTRUCT_ID]);
    assert!(successors("com.altnautica.follow-me", "drone").is_empty());
}

#[test]
fn a_disabled_retired_extension_hands_over_to_a_successor_that_stays_off() {
    let dir = tempfile::tempdir().unwrap();
    let mut sup = supervisor(dir.path());
    install_local(&mut sup, dir.path(), VISION_NAV_ID, false);
    let (catalog, source) = navigation_only();

    let done = retirements(&mut sup, &catalog, &source, Duration::ZERO, Duration::ZERO);
    assert!(
        matches!(
            &done[..],
            [Retirement::Replaced {
                retired: VISION_NAV_ID,
                ..
            }]
        ),
        "{done:?}"
    );
    sup.discover().unwrap();
    assert!(sup.find_install(VISION_NAV_ID).is_none());
    let nav = sup.find_install(NAVIGATION_ID).unwrap();
    assert_eq!(
        nav.status,
        PluginStatus::Installed,
        "installed but not enabled"
    );
    assert!(
        nav.permissions
            .get("hardware.spi")
            .is_some_and(|g| g.granted),
        "the required grant is recorded"
    );
}

#[test]
fn world_engine_stays_off_when_navigation_installs_but_capture_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let mut sup = supervisor(dir.path());
    install_local(&mut sup, dir.path(), VISION_NAV_ID, true);
    install_local(&mut sup, dir.path(), WORLD_ENGINE_ID, true);
    let (catalog, source) = navigation_only();

    let done = retirements(&mut sup, &catalog, &source, Duration::ZERO, Duration::ZERO);
    assert!(
        matches!(
            &done[..],
            [
                Retirement::Replaced {
                    retired: VISION_NAV_ID,
                    ..
                },
                Retirement::Kept {
                    retired: WORLD_ENGINE_ID,
                    ..
                },
            ]
        ),
        "{done:?}"
    );
    sup.discover().unwrap();
    assert!(is_on(&sup, NAVIGATION_ID));
    assert!(
        !is_on(&sup, WORLD_ENGINE_ID),
        "world-engine never runs beside Navigation"
    );
    assert!(sup.find_install(WORLD_ENGINE_ID).is_some());
    assert!(sup.find_install(VISION_NAV_ID).is_none());
    assert!(report(&done[1]), "the kept world-engine is reported");
}

#[test]
fn world_engine_comes_back_on_when_none_of_its_successors_install() {
    let dir = tempfile::tempdir().unwrap();
    let mut sup = supervisor(dir.path());
    install_local(&mut sup, dir.path(), WORLD_ENGINE_ID, true);

    let done = retirements(
        &mut sup,
        r#"{"schema_version":1,"plugins":[]}"#,
        &StaticDownloadSource::default(),
        Duration::ZERO,
        Duration::ZERO,
    );
    assert!(
        matches!(
            &done[..],
            [Retirement::Kept {
                retired: WORLD_ENGINE_ID,
                ..
            }]
        ),
        "{done:?}"
    );
    sup.discover().unwrap();
    assert!(is_on(&sup, WORLD_ENGINE_ID));
}

#[test]
fn the_successor_replaces_the_retired_extension_and_its_data_stays() {
    let dir = tempfile::tempdir().unwrap();
    let mut sup = with_vision_nav(dir.path());
    let data = dir.path().join("plugin-data").join(VISION_NAV_ID);
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("keep"), b"x").unwrap();
    let body = archive(NAVIGATION_ID);
    let sha = hex::encode(Sha256::digest(&body));
    let source = StaticDownloadSource::default().with(URL, body);

    let done = retirements(
        &mut sup,
        &catalog(URL, &sha),
        &source,
        Duration::ZERO,
        Duration::ZERO,
    );
    assert!(
        matches!(
            &done[..],
            [Retirement::Replaced {
                retired: VISION_NAV_ID,
                successors,
                ..
            }] if *successors == [NAVIGATION_ID]
        ),
        "{done:?}"
    );
    sup.discover().unwrap();
    assert!(sup.find_install(VISION_NAV_ID).is_none());
    let nav = sup.find_install(NAVIGATION_ID).unwrap();
    assert!(
        matches!(nav.status, PluginStatus::Enabled | PluginStatus::Running),
        "{:?}",
        nav.status
    );
    assert!(
        data.join("keep").exists(),
        "the retired extension's data is kept"
    );
    assert!(!report(&done[0]));
}

#[test]
fn with_no_release_yet_the_retired_extension_keeps_running() {
    let dir = tempfile::tempdir().unwrap();
    let mut sup = with_vision_nav(dir.path());
    let done = retirements(
        &mut sup,
        r#"{"schema_version":1,"plugins":[]}"#,
        &StaticDownloadSource::default(),
        Duration::ZERO,
        Duration::ZERO,
    );
    assert!(
        matches!(
            &done[..],
            [Retirement::Kept {
                retired: VISION_NAV_ID,
                ..
            }]
        ),
        "{done:?}"
    );
    sup.discover().unwrap();
    let kept = sup.find_install(VISION_NAV_ID).unwrap();
    assert!(
        matches!(kept.status, PluginStatus::Enabled | PluginStatus::Running),
        "{:?}",
        kept.status
    );
    assert!(sup.find_install(NAVIGATION_ID).is_none());
    assert!(
        report(&done[0]),
        "a kept retired extension is reported to the operator"
    );
}

#[test]
fn a_disabled_retired_extension_stays_off_when_it_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let mut sup = with_vision_nav(dir.path());
    sup.disable(VISION_NAV_ID).unwrap();
    retire_all(
        &mut sup,
        r#"{"schema_version":1,"plugins":[]}"#,
        &StaticDownloadSource::default(),
        Duration::ZERO,
        Duration::ZERO,
    );
    sup.discover().unwrap();
    assert_eq!(
        sup.find_install(VISION_NAV_ID).unwrap().status,
        PluginStatus::Disabled
    );
}

#[test]
fn a_node_without_retired_extensions_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let mut sup = supervisor(dir.path());
    let done = retirements(
        &mut sup,
        &catalog("", ""),
        &StaticDownloadSource::default(),
        Duration::ZERO,
        Duration::ZERO,
    );
    assert!(done.is_empty());
}
