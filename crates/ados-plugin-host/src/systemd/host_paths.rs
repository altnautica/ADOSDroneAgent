//! Render-time host lookups a plugin unit depends on: the flight-controller
//! serial port to mask and the capture root to bind.

use std::path::{Path, PathBuf};

use crate::sandbox::configured_fc_serial_port;

/// The run-dir sidecar the plugin-host daemon records the router's live FC
/// device in (the `fc_device` state extra), so every process that renders a
/// plugin unit masks the port the router actually latched, not only a
/// configured one.
pub const FC_DEVICE_SIDECAR: &str = "plugin-fc-device";

/// The flight-controller serial port to mask, resolved at render time from the
/// run dir (`ADOS_RUN_DIR`) and the agent config (`ADOS_CONFIG`).
pub(crate) fn fc_serial_port() -> Option<String> {
    let run_dir = std::env::var_os("ADOS_RUN_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(crate::supervisor::DEFAULT_RUN_DIR));
    let config = std::env::var("ADOS_CONFIG").unwrap_or_else(|_| "/etc/ados/config.yaml".into());
    fc_serial_port_in(&run_dir, Path::new(&config))
}

/// The host capture root to bind into a capture plugin, resolved at render
/// time: the janitor's sidecar (`capture-storage` in the path registry, under
/// the run dir), else the persisted `auto` choice, else the fallback root.
pub(crate) fn capture_root() -> Option<String> {
    let run_dir = std::env::var_os("ADOS_RUN_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(crate::supervisor::DEFAULT_RUN_DIR));
    Some(capture_root_in(
        &run_dir,
        Path::new(ados_config::capture_storage::STICKY_ROOT_FILE),
    ))
}

/// [`capture_root`] against explicit locations, for tests.
pub fn capture_root_in(run_dir: &Path, sticky: &Path) -> String {
    #[derive(serde::Deserialize)]
    struct Root {
        capture_root: String,
    }
    std::fs::read(ados_config::capture_storage::sidecar_path_in(run_dir))
        .ok()
        .and_then(|b| serde_json::from_slice::<Root>(&b).ok())
        .map(|r| r.capture_root)
        .filter(|r| !r.is_empty())
        .or_else(|| ados_config::capture_storage::read_sticky(sticky).map(|s| s.root))
        .unwrap_or_else(|| ados_config::capture_storage::FALLBACK_ROOT.to_string())
}

/// The FC port: the router's live device from the sidecar under `run_dir`
/// first, else the configured `/dev/*` port in `config`, canonicalized so a
/// symlink such as `/dev/serial0` masks the node it points at.
pub fn fc_serial_port_in(run_dir: &Path, config: &Path) -> Option<String> {
    let port = read_live_fc_device(run_dir).or_else(|| {
        std::fs::read_to_string(config)
            .ok()
            .and_then(|yaml| configured_fc_serial_port(&yaml))
    })?;
    Some(canonical_device(&port))
}

/// The live FC device recorded under `run_dir`, when it names a `/dev` path.
pub fn read_live_fc_device(run_dir: &Path) -> Option<String> {
    let body = std::fs::read_to_string(run_dir.join(FC_DEVICE_SIDECAR)).ok()?;
    let device = body.trim();
    device.starts_with("/dev/").then(|| device.to_string())
}

/// Record the router's live FC device under `run_dir`. Returns whether the
/// recorded value changed (the caller re-renders the plugin units then).
pub fn record_live_fc_device(run_dir: &Path, device: &str) -> std::io::Result<bool> {
    let device = canonical_device(device);
    if read_live_fc_device(run_dir).as_deref() == Some(device.as_str()) {
        return Ok(false);
    }
    ados_protocol::sidecar::write_atomic(
        &run_dir.join(FC_DEVICE_SIDECAR),
        format!("{device}\n").as_bytes(),
        0o644,
    )?;
    Ok(true)
}

pub(super) fn canonical_device(port: &str) -> String {
    std::fs::canonicalize(port)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| port.to_string())
}
