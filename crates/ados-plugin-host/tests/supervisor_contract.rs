//! Consumer-facing contract test.
//!
//! Drives the full plugin lifecycle through the crate's PUBLIC API only (no
//! crate-internal paths), with an injected recording `systemctl`. This is the
//! seam the agent's other in-process services depend on (install → grant →
//! enable → disable → remove); the test fails to compile if any required type
//! or method stops being public, which is the point.

use std::io::{Cursor, Write};
use std::path::Path;
use std::sync::Arc;

use ados_plugin_host::archive::parse_archive_bytes;
use ados_plugin_host::supervisor::RecordingSystemctl;
use ados_plugin_host::{Paths, PluginStatus, PluginSupervisor};
use zip::write::SimpleFileOptions;

const PLUGIN_ID: &str = "com.example.contract";
const MANIFEST: &str = "id: com.example.contract\nversion: 1.0.0\nrisk: low\ncompatibility:\n  ados_version: \">=0.1.0,<99.0.0\"\nagent:\n  entrypoint: agent/py/x.py\n  permissions:\n    - event.publish\n";

fn build_archive() -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut w = zip::ZipWriter::new(Cursor::new(&mut buf));
        let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        w.start_file("manifest.yaml", opts).unwrap();
        w.write_all(MANIFEST.as_bytes()).unwrap();
        w.start_file("agent/py/x.py", opts).unwrap();
        w.write_all(b"print('hi')").unwrap();
        w.finish().unwrap();
    }
    buf
}

fn paths_in(dir: &Path) -> Paths {
    Paths {
        install_dir: dir.join("plugins"),
        unit_dir: dir.join("units"),
        state_path: dir.join("state/plugin-state.json"),
        log_dir: dir.join("logs"),
    }
}

#[test]
fn full_lifecycle_via_public_api() {
    let dir = tempfile::tempdir().unwrap();
    let rec = Arc::new(RecordingSystemctl::default());
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "1.0.0")
        .with_systemctl(rec.clone());

    // install (unsigned accepted because require_signed=false).
    let archive = parse_archive_bytes(build_archive()).expect("parse archive");
    let res = sup
        .install_contents(archive, Path::new("/tmp/contract.adosplug"))
        .expect("install");
    assert_eq!(res.plugin_id, PLUGIN_ID);
    assert_eq!(res.version, "1.0.0");
    assert!(res
        .permissions_requested
        .contains(&"event.publish".to_string()));

    // the requested permission starts ungranted; grant it.
    sup.grant_permission(PLUGIN_ID, "event.publish")
        .expect("grant");

    // enable → the install is enabled/running and present in installs().
    sup.enable(PLUGIN_ID).expect("enable");
    let inst = sup.find_install(PLUGIN_ID).expect("installed");
    assert!(matches!(
        inst.status,
        PluginStatus::Enabled | PluginStatus::Running
    ));
    assert!(sup.installs().iter().any(|i| i.plugin_id == PLUGIN_ID));

    // disable → status flips to disabled.
    sup.disable(PLUGIN_ID).expect("disable");
    assert!(matches!(
        sup.find_install(PLUGIN_ID).expect("still installed").status,
        PluginStatus::Disabled
    ));

    // remove → the install is gone.
    sup.remove(PLUGIN_ID, false).expect("remove");
    assert!(sup.find_install(PLUGIN_ID).is_none());
}
