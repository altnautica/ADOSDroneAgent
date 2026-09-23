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
//!
//! **Where the log bound comes from.** The unit sends stdout and stderr to
//! `StandardOutput=append:<log>`, and systemd has no size limit for that
//! destination — there is no directive that caps it and nothing rotates it, so
//! for a long time nothing on the box bounded these files at all. The size cap
//! is enforced from outside, by the supervisor's disk janitor, which trims any
//! `*.log` under [`PLUGIN_LOG_DIR`] past [`PLUGIN_LOG_MAX_BYTES`] while keeping
//! its tail.
//!
//! It trims in place rather than renaming, and that constraint belongs here as
//! much as there: systemd opens the append file **once**, when the unit starts,
//! and holds that descriptor for the life of the plugin. A rotation that renamed
//! the file would leave every later write going to the renamed inode, so the
//! rotated log would keep growing under its new name while the fresh one stayed
//! empty forever. Anything that changes this filename scheme has to keep the
//! janitor's glob matching, or these go unbounded again.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::errors::SupervisorError;
use crate::manifest::{AgentIsolation, AgentRuntime, PluginManifest};
use crate::sandbox::sandbox_directives;
use crate::server::{plugin_socket_dir, plugin_socket_path, DEFAULT_SOCKET_DIR};

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
/// Suffix every generated plugin log carries. The janitor selects files to trim
/// by this suffix, so it is part of the contract between the two, not a
/// cosmetic detail of the filename.
pub const PLUGIN_LOG_SUFFIX: &str = ".log";
/// The size a plugin log is allowed to reach before it is trimmed.
///
/// Declared here, next to the writer, because this is the only place that knows
/// these files exist; enforced by the supervisor's disk janitor, because nothing
/// in a systemd unit can express a cap on an `append:` destination. A plugin
/// logging a line per second at a hundred bytes takes about four days to reach
/// it, which leaves the window an operator actually reads while bounding what a
/// chatty or looping plugin can do to the card.
pub const PLUGIN_LOG_MAX_BYTES: u64 = 32 * 1024 * 1024;

/// The shared cgroup slice file content.
///
/// `IOWeight=10` against the default 100 the flight units run at: systemd
/// cannot bound the bandwidth of an `append:` log destination (see the module
/// note above), so I/O arbitration is the only lever on a plugin that writes
/// hard. A plugin loses every contended block against `ados-mavlink` and
/// `ados-video` rather than delaying telemetry.
pub const PLUGIN_SLICE_CONTENT: &str = "\
[Unit]
Description=ADOS plugin shared cgroup slice
Before=slices.target

[Slice]
CPUAccounting=yes
MemoryAccounting=yes
TasksAccounting=yes
IOAccounting=yes
IOWeight=10
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
///
/// Directory and suffix come from the constants rather than being spelled out,
/// because the janitor that bounds these files finds them by exactly that
/// directory and suffix. Writing either literally here would let the two drift
/// apart with nothing failing until a card filled.
fn log_path_for(plugin_id: &str) -> String {
    format!(
        "{PLUGIN_LOG_DIR}/{}{PLUGIN_LOG_SUFFIX}",
        sanitize_unit_name(plugin_id)
    )
}

/// Render the per-plugin systemd unit. Returns `Ok(None)` for plugins that need
/// no unit (no agent half, or `inprocess` isolation) — the caller treats that as
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
///
/// Every value spliced into the unit text must be a single token: whitespace
/// would split an `ExecStart` argument and a newline or other control character
/// would start a new directive (an `ExecStartPre=+...` runs as root). A value
/// that is not is refused, never escaped into a unit systemd would still run.
///
/// `loopback_guard_active` is the [`crate::loopback_guard`] verdict; without
/// it a `network.outbound` grant renders the no-grant socket policy.
pub fn render_unit(
    manifest: &PluginManifest,
    install_dir: &Path,
    granted: &BTreeSet<String>,
    loopback_guard_active: bool,
) -> Result<Option<String>, SupervisorError> {
    let Some(agent) = manifest.agent.as_ref() else {
        return Ok(None);
    };
    if agent.isolation != AgentIsolation::Subprocess {
        return Ok(None);
    }
    unit_token("plugin id", &manifest.id)?;
    let res = &agent.resources;
    let log_path = log_path_for(&manifest.id);
    let socket_dir = plugin_socket_dir(Path::new(DEFAULT_SOCKET_DIR), &manifest.id);
    let socket_path = plugin_socket_path(Path::new(DEFAULT_SOCKET_DIR), &manifest.id);
    let socket_dir = socket_dir.display();
    let socket_path = socket_path.display();
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
            install_dir = unit_token("install dir", &install_dir.display().to_string())?,
            plugin_id = manifest.id,
            entrypoint = unit_token("entrypoint", &agent.entrypoint)?,
            socket_path = socket_path,
        ),
    };
    // Token delivery: a 0600 EnvironmentFile carries ADOS_PLUGIN_TOKEN (and
    // ADOS_PLUGIN_SOCKET) into the runner, which reads both from its
    // environment. The file is rewritten with a fresh token on each start and
    // on every rotation, so the `-` prefix tolerates its absence during
    // install (before the first mint) without failing the unit. The runner
    // waits for it rather than degrading, so the optional prefix cannot
    // produce a silently token-less plugin.
    let token_env_file = format!("{DEFAULT_SOCKET_DIR}/{}.token.env", manifest.id);
    // The plugin's own socket directory is the one path under the hidden run
    // dir bound into this unit (read-only; see `crate::sandbox`), so the plugin
    // reaches its own host socket and no other plugin's.
    //
    // The capability-backed half of the sandbox. Everything above the marker is
    // fixed hardening; these lines change with the operator's grants, which is
    // why a grant or revoke re-renders the unit.
    let sandbox = sandbox_directives(granted, loopback_guard_active).join("\n");
    Ok(Some(format!(
        "\
[Unit]
Description=ADOS plugin {plugin_id}
After=ados-supervisor.service
PartOf=ados-supervisor.service
# No start rate limit: a plugin whose host socket is not up yet must keep
# retrying rather than land in a failed state an operator has to clear by hand.
StartLimitIntervalSec=0

[Service]
Slice={slice_name}
Type=simple
Environment=ADOS_PLUGIN_SOCKET={socket_path}
EnvironmentFile=-{token_env_file}
BindReadOnlyPaths={socket_dir}
ExecStart={exec_start}
Restart=on-failure
RestartSec=2s
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
LockPersonality=yes
RestrictRealtime=yes
RestrictSUIDSGID=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectControlGroups=yes
ProtectProc=invisible
RestrictNamespaces=yes
SystemCallArchitectures=native
# ---- capability sandbox (re-rendered on every grant/revoke) ----
{sandbox}

[Install]
WantedBy=ados-supervisor.service
",
        plugin_id = manifest.id,
        slice_name = PLUGIN_SLICE_NAME,
        socket_path = socket_path,
        token_env_file = token_env_file,
        socket_dir = socket_dir,
        exec_start = exec_start,
        max_ram_mb = res.max_ram_mb,
        max_cpu_percent = res.max_cpu_percent,
        max_pids = res.max_pids,
        log_path = log_path,
        sandbox = sandbox,
    )))
}

/// Refuse a value that is not a single unit-file token (see [`render_unit`]).
fn unit_token<'a>(what: &str, value: &'a str) -> Result<&'a str, SupervisorError> {
    if value.is_empty() || value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(SupervisorError(format!(
            "refusing to render a systemd unit: {what} {value:?} is empty or contains \
             whitespace or a control character"
        )));
    }
    Ok(value)
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
    fn the_log_path_stays_where_the_janitor_looks_for_it() {
        // systemd cannot cap an `append:` file, so the only thing bounding these
        // is the supervisor's janitor, which finds them by this directory and
        // this suffix. If either moves without the janitor moving with it, the
        // logs go unbounded again and nothing else here would notice.
        let path = log_path_for("com.example.thermal-lepton");
        assert!(
            path.starts_with(&format!("{PLUGIN_LOG_DIR}/")),
            "plugin logs must stay in the directory the janitor sweeps: {path}"
        );
        assert!(
            path.ends_with(PLUGIN_LOG_SUFFIX),
            "plugin logs must keep the suffix the janitor selects on: {path}"
        );
        // The concrete pair, so a change to either constant is a visible diff
        // rather than a quiet one.
        assert_eq!(PLUGIN_LOG_DIR, "/var/log/ados/plugins");
        assert_eq!(PLUGIN_LOG_SUFFIX, ".log");
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
        let unit = render_unit(
            &subprocess_manifest(),
            Path::new("/var/ados/plugins"),
            &BTreeSet::new(),
            false,
        )
        .unwrap()
        .unwrap();
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
        let unit = render_unit(&m, Path::new("/var/ados/plugins"), &BTreeSet::new(), false)
            .unwrap()
            .unwrap();
        // ExecStart points at the unpacked plugin binary, the plugin id as the
        // leading positional (the SDK runner requires it), then the socket path.
        assert!(
            unit.contains(
                "ExecStart=/var/ados/plugins/com.example.rustplug/agent/bin/com.example.rustplug com.example.rustplug --socket /run/ados/plugins/com.example.rustplug/host.sock"
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
        assert!(unit.contains(
            "Environment=ADOS_PLUGIN_SOCKET=/run/ados/plugins/com.example.rustplug/host.sock"
        ));
        assert!(unit.contains("EnvironmentFile=-/run/ados/plugins/com.example.rustplug.token.env"));
        // The shared/hardening/limit lines are identical to the python branch.
        assert!(unit.contains("Slice=ados-plugins.slice"));
        assert!(unit.contains("MemoryMax=64M"));
        assert!(unit.contains("NoNewPrivileges=yes"));
        assert!(
            unit.contains("StandardOutput=append:/var/log/ados/plugins/com-example-rustplug.log")
        );
    }

    #[test]
    fn a_unit_reaches_its_own_socket_dir_and_no_other_plugins() {
        // Two plugins on one box. Each unit hides the whole run dir and binds
        // back exactly one plugin directory, its own, which holds the socket
        // its runner is pointed at.
        let render = |id: &str| {
            let m = PluginManifest::from_yaml_text(&format!(
                "id: {id}\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  entrypoint: agent/py/p.py\n"
            ))
            .unwrap();
            render_unit(&m, Path::new("/var/ados/plugins"), &BTreeSet::new(), false)
                .unwrap()
                .unwrap()
        };
        for (id, other) in [
            ("com.example.a", "com.example.b"),
            ("com.example.b", "com.example.a"),
        ] {
            let unit = render(id);
            assert!(unit.contains("TemporaryFileSystem=/run/ados:ro"), "{unit}");
            let binds: Vec<&str> = unit
                .lines()
                .filter_map(|l| l.strip_prefix("BindReadOnlyPaths="))
                .filter(|p| p.contains("/run/ados/plugins"))
                .collect();
            assert_eq!(binds, vec![format!("/run/ados/plugins/{id}")], "{unit}");
            assert!(unit.contains(&format!(
                "Environment=ADOS_PLUGIN_SOCKET=/run/ados/plugins/{id}/host.sock"
            )));
            assert!(
                !unit.contains(&format!("/run/ados/plugins/{other}")),
                "{unit}"
            );
        }
    }

    #[test]
    fn inprocess_and_gcs_only_render_no_unit() {
        let inproc = PluginManifest::from_yaml_text(
            "id: com.altnautica.builtin\nversion: 0.1.0\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  entrypoint: pkg:Class\n  isolation: inprocess\n",
        )
        .unwrap();
        assert!(render_unit(
            &inproc,
            Path::new("/var/ados/plugins"),
            &BTreeSet::new(),
            false
        )
        .unwrap()
        .is_none());

        let gcs_only = PluginManifest::from_yaml_text(
            "id: com.example.panel\nversion: 0.1.0\ncompatibility:\n  ados_version: \">=0.1.0\"\ngcs:\n  entrypoint: gcs/dist/index.js\n",
        )
        .unwrap();
        assert!(render_unit(
            &gcs_only,
            Path::new("/var/ados/plugins"),
            &BTreeSet::new(),
            false
        )
        .unwrap()
        .is_none());
    }

    /// A manifest deserialized without the parser's validation, as one would be
    /// by any path that skips `from_yaml_text`. Values are JSON-quoted YAML
    /// scalars so a newline survives into the struct.
    fn unvalidated_rust_manifest(id: &str, entrypoint: &str) -> PluginManifest {
        let q = |s: &str| serde_json::to_string(s).unwrap();
        serde_norway::from_str(&format!(
            "id: {}\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  entrypoint: {}\n  runtime: rust\n",
            q(id),
            q(entrypoint)
        ))
        .unwrap()
    }

    #[test]
    fn a_value_that_would_split_or_add_a_unit_line_is_refused() {
        let install_dir = Path::new("/var/ados/plugins");
        for (id, entrypoint) in [
            (
                "com.example.x",
                "bin/x\nExecStartPre=+/bin/sh -c 'id>/root/p'",
            ),
            ("com.example.x", "bin/x --extra"),
            ("com.example.x", "bin/x\u{1b}[2J"),
            ("com.example.x\nExecStartPre=+/bin/sh", "bin/x"),
            ("com.example.x y", "bin/x"),
        ] {
            let m = unvalidated_rust_manifest(id, entrypoint);
            let err = render_unit(&m, install_dir, &BTreeSet::new(), false)
                .expect_err(&format!("{id:?} / {entrypoint:?} must be refused"));
            assert!(err.0.contains("refusing to render"), "{}", err.0);
        }
        // An install dir with whitespace is refused on the rust ExecStart too.
        let m = unvalidated_rust_manifest("com.example.x", "bin/x");
        assert!(render_unit(
            &m,
            Path::new("/var/ados/my plugins"),
            &BTreeSet::new(),
            false
        )
        .is_err());
        // The same manifest renders under a clean install dir.
        assert!(render_unit(&m, install_dir, &BTreeSet::new(), false)
            .unwrap()
            .is_some());
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
