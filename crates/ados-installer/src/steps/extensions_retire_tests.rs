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
agent:\n  entrypoint: agent/py/x.py\n  target_profiles: [drone]\n  permissions:\n    - hardware.spi\n"
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

/// A drone with vision-nav installed and running.
fn with_vision_nav(dir: &Path) -> PluginSupervisor {
    let mut sup = supervisor(dir);
    let staged = dir.join("vision-nav.adosplug");
    std::fs::write(&staged, archive(VISION_NAV_ID)).unwrap();
    sup.install_archive(&staged).unwrap();
    sup.grant_permission(VISION_NAV_ID, "hardware.spi").unwrap();
    sup.enable(VISION_NAV_ID).unwrap();
    sup
}

fn catalog(url: &str, sha: &str) -> String {
    serde_json::json!({
        "schema_version": 1,
        "plugins": [{ "id": NAVIGATION_ID, "version": "1.0.0", "download_url": url, "archive_sha256": sha }],
    })
    .to_string()
}

#[test]
fn each_retired_extension_names_its_successor_by_profile() {
    assert_eq!(successor(VISION_NAV_ID, "drone"), Some(NAVIGATION_ID));
    assert_eq!(successor(VISION_NAV_ID, "visor"), Some(NAVIGATION_ID));
    assert_eq!(successor(WORLD_ENGINE_ID, "drone"), Some(CAPTURE_ID));
    assert_eq!(
        successor(WORLD_ENGINE_ID, "ground-station"),
        Some(CAPTURE_ID)
    );
    assert_eq!(
        successor(WORLD_ENGINE_ID, "workstation"),
        Some(RECONSTRUCT_ID)
    );
    assert_eq!(successor(WORLD_ENGINE_ID, "compute"), Some(RECONSTRUCT_ID));
    assert_eq!(successor("com.altnautica.follow-me", "drone"), None);
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

    let done = retire_all(
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
                successor: NAVIGATION_ID,
                ..
            }]
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
    let done = retire_all(
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
        !report(&done[0]),
        "an unpublished successor is not a failure"
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
    let done = retire_all(
        &mut sup,
        &catalog("", ""),
        &StaticDownloadSource::default(),
        Duration::ZERO,
        Duration::ZERO,
    );
    assert!(done.is_empty());
}
