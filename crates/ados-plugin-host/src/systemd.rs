//! Systemd unit generation for subprocess plugins.
//!
//! Each subprocess plugin runs as a generated systemd service
//! `ados-plugin-<id>.service` inside the shared `ados-plugins.slice` cgroup
//! slice. Restart, watchdog, and resource limits come from systemd; there is no
//! manual cgroupv2 management. Built-in `inprocess` plugins skip this entirely.
//!
//! These are pure string builders. The byte-exact `Slice=`, hardening flags,
//! `ExecStart`, and resource limits are asserted in tests; nothing here invokes
//! systemd.

use std::path::{Path, PathBuf};

use crate::manifest::{AgentIsolation, PluginManifest};

/// Path to the per-plugin runner binary that systemd starts.
pub const PLUGIN_RUNNER_BINARY: &str = "/opt/ados/venv/bin/ados-plugin-runner";
/// The shared slice name.
pub const PLUGIN_SLICE_NAME: &str = "ados-plugins.slice";
/// Directory units and the slice file are written to.
pub const PLUGIN_UNIT_DIR: &str = "/etc/systemd/system";
/// Prefix on every generated per-plugin unit file.
pub const PLUGIN_UNIT_PREFIX: &str = "ados-plugin-";
/// Directory plugin logs are appended to.
pub const PLUGIN_LOG_DIR: &str = "/var/log/ados/plugins";

/// The shared cgroup slice file content.
pub const PLUGIN_SLICE_CONTENT: &str = "\
[Unit]
Description=ADOS plugin shared cgroup slice
Before=slices.target

[Slice]
CPUAccounting=yes
MemoryAccounting=yes
TasksAccounting=yes
IOAccounting=yes
";

/// Return the slice file content.
pub fn slice_unit_content() -> &'static str {
    PLUGIN_SLICE_CONTENT
}

/// Absolute path of the slice file under the unit dir.
pub fn slice_unit_path(unit_dir: Option<&Path>) -> PathBuf {
    let dir = unit_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(PLUGIN_UNIT_DIR));
    dir.join(PLUGIN_SLICE_NAME)
}

/// Convert a reverse-DNS plugin id to a systemd-safe unit basename.
///
/// `com.example.thermal-lepton` becomes `com-example-thermal-lepton`. Periods
/// are not permitted in unit-file basenames before `.service`; hyphens are.
pub fn sanitize_unit_name(plugin_id: &str) -> String {
    plugin_id.replace('.', "-")
}

/// The full unit file name for a plugin, e.g. `ados-plugin-com-example-x.service`.
pub fn unit_name_for(plugin_id: &str) -> String {
    format!(
        "{PLUGIN_UNIT_PREFIX}{}.service",
        sanitize_unit_name(plugin_id)
    )
}

/// The absolute path of a plugin's unit file under the unit dir.
pub fn unit_path_for(plugin_id: &str, unit_dir: Option<&Path>) -> PathBuf {
    let dir = unit_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(PLUGIN_UNIT_DIR));
    dir.join(unit_name_for(plugin_id))
}

/// The append-log path for a plugin's stdout/stderr.
fn log_path_for(plugin_id: &str) -> String {
    format!("{PLUGIN_LOG_DIR}/{}.log", sanitize_unit_name(plugin_id))
}

/// Render the per-plugin systemd unit. Returns `None` for plugins that need no
/// unit (no agent half, or `inprocess` isolation) — the caller treats that as
/// "do not write a unit", mirroring the Python `render_unit` raising for those
/// cases.
pub fn render_unit(manifest: &PluginManifest) -> Option<String> {
    let agent = manifest.agent.as_ref()?;
    if agent.isolation != AgentIsolation::Subprocess {
        return None;
    }
    let res = &agent.resources;
    let log_path = log_path_for(&manifest.id);
    Some(format!(
        "\
[Unit]
Description=ADOS plugin {plugin_id}
After=ados-supervisor.service
PartOf=ados-supervisor.service

[Service]
Slice={slice_name}
Type=simple
ExecStart={runner} {plugin_id}
Restart=on-failure
RestartSec=2s
StartLimitInterval=60s
StartLimitBurst=5
MemoryMax={max_ram_mb}M
CPUQuota={max_cpu_percent}%
TasksMax={max_pids}
StandardOutput=append:{log_path}
StandardError=append:{log_path}
User=ados
Group=ados
NoNewPrivileges=yes
PrivateTmp=yes
ProtectSystem=strict
ProtectHome=yes
ReadWritePaths=/var/ados/plugin-data /var/log/ados/plugins /run/ados/plugins
LockPersonality=yes
RestrictRealtime=yes
RestrictSUIDSGID=yes

[Install]
WantedBy=ados-supervisor.service
",
        plugin_id = manifest.id,
        slice_name = PLUGIN_SLICE_NAME,
        runner = PLUGIN_RUNNER_BINARY,
        max_ram_mb = res.max_ram_mb,
        max_cpu_percent = res.max_cpu_percent,
        max_pids = res.max_pids,
        log_path = log_path,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subprocess_manifest() -> PluginManifest {
        PluginManifest::from_yaml_text(
            "id: com.example.thermal-lepton\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  entrypoint: agent/py/thermal.py\n  resources:\n    max_ram_mb: 128\n    max_cpu_percent: 40\n    max_pids: 16\n",
        )
        .unwrap()
    }

    #[test]
    fn sanitize_replaces_dots_with_hyphens() {
        assert_eq!(
            sanitize_unit_name("com.example.thermal-lepton"),
            "com-example-thermal-lepton"
        );
        assert_eq!(
            unit_name_for("com.example.thermal-lepton"),
            "ados-plugin-com-example-thermal-lepton.service"
        );
    }

    #[test]
    fn unit_contains_slice_hardening_execstart_and_limits() {
        let unit = render_unit(&subprocess_manifest()).unwrap();
        assert!(unit.contains("Slice=ados-plugins.slice"));
        assert!(unit.contains(
            "ExecStart=/opt/ados/venv/bin/ados-plugin-runner com.example.thermal-lepton"
        ));
        assert!(unit.contains("MemoryMax=128M"));
        assert!(unit.contains("CPUQuota=40%"));
        assert!(unit.contains("TasksMax=16"));
        // Hardening flags.
        for flag in [
            "NoNewPrivileges=yes",
            "PrivateTmp=yes",
            "ProtectSystem=strict",
            "ProtectHome=yes",
            "LockPersonality=yes",
            "RestrictRealtime=yes",
            "RestrictSUIDSGID=yes",
        ] {
            assert!(unit.contains(flag), "missing {flag}");
        }
        // Log append path uses the sanitized id.
        assert!(unit.contains(
            "StandardOutput=append:/var/log/ados/plugins/com-example-thermal-lepton.log"
        ));
    }

    #[test]
    fn inprocess_and_gcs_only_render_no_unit() {
        let inproc = PluginManifest::from_yaml_text(
            "id: com.altnautica.builtin\nversion: 0.1.0\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  entrypoint: pkg:Class\n  isolation: inprocess\n",
        )
        .unwrap();
        assert!(render_unit(&inproc).is_none());

        let gcs_only = PluginManifest::from_yaml_text(
            "id: com.example.panel\nversion: 0.1.0\ncompatibility:\n  ados_version: \">=0.1.0\"\ngcs:\n  entrypoint: gcs/dist/index.js\n",
        )
        .unwrap();
        assert!(render_unit(&gcs_only).is_none());
    }

    #[test]
    fn slice_content_has_accounting_block() {
        let s = slice_unit_content();
        assert!(s.contains("[Slice]"));
        assert!(s.contains("CPUAccounting=yes"));
        assert!(s.contains("MemoryAccounting=yes"));
        assert!(s.contains("TasksAccounting=yes"));
        assert!(s.contains("IOAccounting=yes"));
    }
}
