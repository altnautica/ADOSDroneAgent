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

use crate::manifest::{AgentIsolation, AgentRuntime, PluginManifest};
use crate::server::DEFAULT_SOCKET_DIR;

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
///
/// The `ExecStart` line is the only part that differs by `agent.runtime`:
/// * `python` (default): the shared Python runner is started with the plugin id.
/// * `rust`: the plugin's own binary is exec'd directly with its socket path.
///
/// `install_dir` is the unpacked-plugin install root (e.g. `/var/ados/plugins`);
/// the rust `ExecStart` resolves to `{install_dir}/{id}/{entrypoint}`. The slice,
/// hardening, limits, and log lines are identical for both runtimes.
pub fn render_unit(manifest: &PluginManifest, install_dir: &Path) -> Option<String> {
    let agent = manifest.agent.as_ref()?;
    if agent.isolation != AgentIsolation::Subprocess {
        return None;
    }
    let res = &agent.resources;
    let log_path = log_path_for(&manifest.id);
    let socket_path = format!("{DEFAULT_SOCKET_DIR}/{}.sock", manifest.id);
    let exec_start = match agent.runtime {
        // Python (default): the shared runner takes the plugin id and resolves
        // the manifest + entrypoint itself. Unchanged.
        AgentRuntime::Python => format!("{PLUGIN_RUNNER_BINARY} {}", manifest.id),
        // Rust: exec the plugin's own binary directly with the plugin id as the
        // leading positional argument (the SDK runner reads it positionally;
        // it is non-secret and already in the install path). The capability
        // token and socket path are delivered via the unit environment, never
        // on the command line (a /proc/<pid>/cmdline is world-readable).
        AgentRuntime::Rust => format!(
            "{install_dir}/{plugin_id}/{entrypoint} {plugin_id} --socket {socket_path}",
            install_dir = install_dir.display(),
            plugin_id = manifest.id,
            entrypoint = agent.entrypoint,
            socket_path = socket_path,
        ),
    };
    // Token delivery: a 0600 EnvironmentFile carries ADOS_PLUGIN_TOKEN (and
    // ADOS_PLUGIN_SOCKET) into the runner, which reads both from its
    // environment. The file is rewritten with a fresh token on each start, so
    // the `-` prefix tolerates its absence during install (before the first
    // mint) without failing the unit. The socket path is also a static
    // Environment line as a belt-and-suspenders fallback for the env-file race.
    let token_env_file = format!("{DEFAULT_SOCKET_DIR}/{}.token.env", manifest.id);
    Some(format!(
        "\
[Unit]
Description=ADOS plugin {plugin_id}
After=ados-supervisor.service
PartOf=ados-supervisor.service

[Service]
Slice={slice_name}
Type=simple
Environment=ADOS_PLUGIN_SOCKET={socket_path}
EnvironmentFile=-{token_env_file}
ExecStart={exec_start}
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
        socket_path = socket_path,
        token_env_file = token_env_file,
        exec_start = exec_start,
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
        let unit = render_unit(&subprocess_manifest(), Path::new("/var/ados/plugins")).unwrap();
        assert!(unit.contains("Slice=ados-plugins.slice"));
        // Python runtime (default): the shared runner takes the plugin id.
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
    fn rust_runtime_unit_execs_the_plugin_binary() {
        let m = PluginManifest::from_yaml_text(
            "id: com.example.rustplug\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  entrypoint: agent/bin/com.example.rustplug\n  runtime: rust\n  resources:\n    max_ram_mb: 64\n    max_cpu_percent: 30\n    max_pids: 8\n",
        )
        .unwrap();
        let unit = render_unit(&m, Path::new("/var/ados/plugins")).unwrap();
        // ExecStart points at the unpacked plugin binary, the plugin id as the
        // leading positional (the SDK runner requires it), then the socket path.
        assert!(
            unit.contains(
                "ExecStart=/var/ados/plugins/com.example.rustplug/agent/bin/com.example.rustplug com.example.rustplug --socket /run/ados/plugins/com.example.rustplug.sock"
            ),
            "{unit}"
        );
        // The token is never on the ExecStart line (it comes from the env file).
        let exec_line = unit
            .lines()
            .find(|l| l.starts_with("ExecStart="))
            .expect("ExecStart line");
        assert!(!exec_line.contains("ADOS_PLUGIN_TOKEN"), "{exec_line}");
        assert!(!exec_line.to_lowercase().contains("token"), "{exec_line}");
        // Token delivery: the socket is a static Environment line and the token
        // rides in an owner-only EnvironmentFile (the `-` prefix tolerates its
        // absence before the first mint).
        assert!(unit
            .contains("Environment=ADOS_PLUGIN_SOCKET=/run/ados/plugins/com.example.rustplug.sock"));
        assert!(unit
            .contains("EnvironmentFile=-/run/ados/plugins/com.example.rustplug.token.env"));
        // The shared/hardening/limit lines are identical to the python branch.
        assert!(unit.contains("Slice=ados-plugins.slice"));
        assert!(unit.contains("MemoryMax=64M"));
        assert!(unit.contains("NoNewPrivileges=yes"));
        assert!(
            unit.contains("StandardOutput=append:/var/log/ados/plugins/com-example-rustplug.log")
        );
    }

    #[test]
    fn inprocess_and_gcs_only_render_no_unit() {
        let inproc = PluginManifest::from_yaml_text(
            "id: com.altnautica.builtin\nversion: 0.1.0\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  entrypoint: pkg:Class\n  isolation: inprocess\n",
        )
        .unwrap();
        assert!(render_unit(&inproc, Path::new("/var/ados/plugins")).is_none());

        let gcs_only = PluginManifest::from_yaml_text(
            "id: com.example.panel\nversion: 0.1.0\ncompatibility:\n  ados_version: \">=0.1.0\"\ngcs:\n  entrypoint: gcs/dist/index.js\n",
        )
        .unwrap();
        assert!(render_unit(&gcs_only, Path::new("/var/ados/plugins")).is_none());
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
