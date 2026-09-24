//! Unit generation for subprocess plugins.
//!
//! Each subprocess plugin runs as a generated service `ados-plugin-<id>.service`
//! inside the shared `ados-plugins.slice` cgroup slice (on systemd; launchd
//! runs the same spec as a LaunchAgent, see [`crate::backend`]). Restart,
//! watchdog, and resource limits come from the service manager; there is no
//! manual cgroupv2 management. Built-in `inprocess` plugins skip this entirely.
//!
//! [`build_unit_spec`] is a pure builder: it resolves the exec line, the runner
//! environment, the binds and the capability sandbox into a
//! [`UnitSpec`](crate::backend::UnitSpec). The byte-exact `Slice=`, hardening
//! flags, `ExecStart`, and resource limits are asserted in tests; nothing here
//! invokes a service manager.
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

use crate::backend::UnitSpec;
use crate::errors::SupervisorError;
use crate::manifest::{bin_reference, host_arch_os, AgentIsolation, AgentRuntime, PluginManifest};
use crate::sandbox::sandbox_directives;
use crate::server::{plugin_socket_dir, plugin_socket_path};
use crate::supervisor::Paths;
use crate::token_secret::token_env_path;

/// Default path of the per-plugin runner binary a Python plugin's unit starts
/// (`ADOS_PLUGIN_RUNNER` overrides it).
pub const PLUGIN_RUNNER_BINARY: &str = "/opt/ados/venv/bin/ados-plugin-runner";
/// The shared slice name.
pub const PLUGIN_SLICE_NAME: &str = "ados-plugins.slice";
/// Default directory units and the slice file are written to
/// (`ADOS_PLUGIN_UNIT_DIR` overrides it).
pub const PLUGIN_UNIT_DIR: &str = "/etc/systemd/system";
/// Prefix on every generated per-plugin unit file.
pub const PLUGIN_UNIT_PREFIX: &str = "ados-plugin-";
/// Default directory plugin logs are appended to (`ADOS_PLUGIN_LOG_DIR`
/// overrides it).
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

/// The env var carrying the plugin's host socket path.
pub const ENV_PLUGIN_SOCKET: &str = "ADOS_PLUGIN_SOCKET";
/// The env var carrying this node's profile (`drone`, `ground-station`,
/// `workstation`, `compute`), so a plugin that behaves per profile needs no
/// read of the agent's config files.
pub const ENV_NODE_PROFILE: &str = "ADOS_NODE_PROFILE";
/// The env var carrying where an `agent.http` plugin serves its HTTP socket.
pub const ENV_PLUGIN_HTTP_SOCKET: &str = "ADOS_PLUGIN_HTTP_SOCKET";
/// The run-dir subdirectory holding each `agent.http` plugin's socket dir.
pub const PLUGIN_HTTP_SUBDIR: &str = "plugin-http";
/// The HTTP socket file inside a plugin's HTTP dir.
pub const PLUGIN_HTTP_SOCKET_NAME: &str = "http.sock";

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

/// Convert a reverse-DNS plugin id to a systemd-safe unit basename.
///
/// `com.example.thermal-lepton` becomes `com-example-thermal-lepton`. Periods
/// are not permitted in unit-file basenames before `.service`; hyphens are.
pub fn sanitize_unit_name(plugin_id: &str) -> String {
    plugin_id.replace('.', "-")
}

/// The full unit name for a plugin, e.g. `ados-plugin-com-example-x.service`.
pub fn unit_name_for(plugin_id: &str) -> String {
    format!(
        "{PLUGIN_UNIT_PREFIX}{}.service",
        sanitize_unit_name(plugin_id)
    )
}

/// The append-log path for a plugin's stdout/stderr under `log_dir`.
///
/// The suffix comes from the constant rather than being spelled out, because
/// the janitor that bounds these files finds them by exactly that directory and
/// suffix. Writing either literally here would let the two drift apart with
/// nothing failing until a card filled.
pub fn log_path_for(log_dir: &Path, plugin_id: &str) -> PathBuf {
    log_dir.join(format!(
        "{}{PLUGIN_LOG_SUFFIX}",
        sanitize_unit_name(plugin_id)
    ))
}

/// The append-log path for one declared service, in the same directory and
/// with the same suffix the janitor bounds.
pub fn service_log_path_for(log_dir: &Path, plugin_id: &str, service_name: &str) -> PathBuf {
    log_dir.join(format!(
        "{}-{}{PLUGIN_LOG_SUFFIX}",
        sanitize_unit_name(plugin_id),
        sanitize_unit_name(service_name)
    ))
}

/// The directory an `agent.http` plugin serves its HTTP socket in:
/// `<run dir>/plugin-http/<id>/`.
pub fn plugin_http_dir(run_dir: &Path, plugin_id: &str) -> PathBuf {
    run_dir.join(PLUGIN_HTTP_SUBDIR).join(plugin_id)
}

/// Build the per-plugin main unit. Returns `Ok(None)` for plugins that need no
/// unit (no agent half, or `inprocess` isolation) — the caller treats that as
/// "do not write a unit".
///
/// The exec line is the only part that differs by `agent.runtime`:
/// * `python` (default): the shared Python runner is started with the plugin id.
/// * `rust`: the plugin's own binary is exec'd directly with its socket path;
///   a `bin:<name>` entrypoint resolves to this host's architecture's entry of
///   `agent.binaries`.
///
/// Every value spliced into the unit text must be a single token: whitespace
/// would split an `ExecStart` argument and a newline or other control character
/// would start a new directive (an `ExecStartPre=+...` runs as root). A value
/// that is not is refused, never escaped into a unit systemd would still run.
///
/// `loopback_guard_active` is the [`crate::loopback_guard`] verdict; without
/// it a `network.outbound` grant renders the no-grant socket policy. The main
/// unit never listens, so it gets no `SocketBindAllow=`.
pub fn build_unit_spec(
    manifest: &PluginManifest,
    paths: &Paths,
    profile: &str,
    granted: &BTreeSet<String>,
    loopback_guard_active: bool,
) -> Result<Option<UnitSpec>, SupervisorError> {
    let Some(agent) = manifest.agent.as_ref() else {
        return Ok(None);
    };
    if agent.isolation != AgentIsolation::Subprocess {
        return Ok(None);
    }
    unit_token("plugin id", &manifest.id)?;
    let socket_path = path_token(
        "socket path",
        &plugin_socket_path(&paths.socket_dir, &manifest.id),
    )?;
    let argv = match agent.runtime {
        // Python (default): the shared runner takes the plugin id and resolves
        // the manifest + entrypoint itself.
        AgentRuntime::Python => vec![path_token("runner", &paths.runner)?, manifest.id.clone()],
        // Rust: exec the plugin's own binary directly with the plugin id as the
        // leading positional argument (the SDK runner reads it positionally;
        // it is non-secret and already in the install path). The capability
        // token and socket path are delivered via the unit environment, never
        // on the command line (a /proc/<pid>/cmdline is world-readable).
        AgentRuntime::Rust => vec![
            resolve_program(manifest, &agent.entrypoint, paths)?,
            manifest.id.clone(),
            "--socket".to_string(),
            socket_path.clone(),
        ],
    };
    let mut spec = runner_context(manifest, paths, profile)?;
    spec.description = format!("ADOS plugin {}", manifest.id);
    spec.argv = argv;
    spec.log_path = log_path_for(&paths.log_dir, &manifest.id);
    spec.restart = "on-failure".to_string();
    spec.resources = agent.resources.clone();
    // The capability-backed half of the sandbox: these lines change with the
    // operator's grants, which is why a grant or revoke re-renders the unit.
    spec.sandbox_directives = sandbox_directives(granted, loopback_guard_active, &[]);
    Ok(Some(spec))
}

/// The part every unit of a plugin shares, main and declared services alike:
/// the host socket in the environment, the token file, and the binds. A
/// declared service runs as the plugin's identity, so it reaches the host SDK
/// exactly as the main process does.
///
/// Token delivery: a 0600 environment file carries `ADOS_PLUGIN_TOKEN` (and
/// `ADOS_PLUGIN_SOCKET`) into the process, which reads both from its
/// environment. The file is rewritten with a fresh token on each start and on
/// every rotation, so it is optional at start (before the first mint) without
/// failing the unit; the runner waits for it rather than degrading, so the
/// optional file cannot produce a silently token-less plugin.
///
/// The plugin's own socket directory is the one path under the hidden run dir
/// bound into the unit (read-only; see [`crate::sandbox`]), so the plugin
/// reaches its own host socket and no other plugin's. An `agent.http` plugin
/// also gets its HTTP dir bound read-write.
pub(crate) fn runner_context(
    manifest: &PluginManifest,
    paths: &Paths,
    profile: &str,
) -> Result<UnitSpec, SupervisorError> {
    let socket_dir = plugin_socket_dir(&paths.socket_dir, &manifest.id);
    let socket_path = plugin_socket_path(&paths.socket_dir, &manifest.id);
    let token_file = token_env_path(&manifest.id, Some(&paths.socket_dir));
    path_token("socket dir", &socket_dir)?;
    path_token("token file", &token_file)?;
    let mut env = vec![
        (
            ENV_PLUGIN_SOCKET.to_string(),
            path_token("socket path", &socket_path)?,
        ),
        (
            ENV_NODE_PROFILE.to_string(),
            unit_token("node profile", profile)?.to_string(),
        ),
    ];
    let mut bind_read_write = Vec::new();
    if manifest.agent.as_ref().is_some_and(|a| a.http) {
        let http_dir = plugin_http_dir(&paths.run_dir, &manifest.id);
        env.push((
            ENV_PLUGIN_HTTP_SOCKET.to_string(),
            path_token("http socket", &http_dir.join(PLUGIN_HTTP_SOCKET_NAME))?,
        ));
        bind_read_write.push(http_dir);
    }
    Ok(UnitSpec {
        description: String::new(),
        argv: Vec::new(),
        working_dir: None,
        env,
        env_file: Some(token_file),
        bind_read_only: vec![socket_dir],
        bind_read_write,
        log_path: PathBuf::new(),
        restart: String::new(),
        resources: Default::default(),
        sandbox_directives: Vec::new(),
    })
}

/// Resolve an executable reference to its absolute path in the plugin tree: a
/// `bin:<name>` through `agent.binaries` for this host's `<arch>-<os>`, any
/// other relative path under the plugin's install dir.
pub(crate) fn resolve_program(
    manifest: &PluginManifest,
    reference: &str,
    paths: &Paths,
) -> Result<String, SupervisorError> {
    let relative = match bin_reference(reference) {
        Some(name) => {
            let arch_os = host_arch_os();
            manifest
                .agent
                .as_ref()
                .and_then(|a| a.binary_path(name, &arch_os))
                .ok_or_else(|| SupervisorError(format!("incompatible: no binary for {arch_os}")))?
        }
        None => reference,
    };
    unit_token("entrypoint", relative)?;
    path_token(
        "install dir",
        &paths.install_dir.join(&manifest.id).join(relative),
    )
}

/// Refuse a value that is not a single unit-file token (see
/// [`build_unit_spec`]).
pub(crate) fn unit_token<'a>(what: &str, value: &'a str) -> Result<&'a str, SupervisorError> {
    if value.is_empty() || value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(SupervisorError(format!(
            "refusing to render a systemd unit: {what} {value:?} is empty or contains \
             whitespace or a control character"
        )));
    }
    Ok(value)
}

/// [`unit_token`] over a path, returning its text.
pub(crate) fn path_token(what: &str, path: &Path) -> Result<String, SupervisorError> {
    let text = path.to_string_lossy();
    unit_token(what, &text).map(str::to_string)
}

/// The fixed hardening every plugin-authored process runs under: the main
/// runner unit, each declared service unit, and each readiness probe.
pub const HARDENING_DIRECTIVES: &[&str] = &[
    "NoNewPrivileges=yes",
    "PrivateTmp=yes",
    "ProtectSystem=strict",
    "LockPersonality=yes",
    "RestrictRealtime=yes",
    "RestrictSUIDSGID=yes",
    "ProtectKernelTunables=yes",
    "ProtectKernelModules=yes",
    "ProtectControlGroups=yes",
    "ProtectProc=invisible",
    "RestrictNamespaces=yes",
    "SystemCallArchitectures=native",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::render_systemd;

    /// The production layout, independent of the test process environment.
    fn fhs() -> Paths {
        crate::supervisor::tests_support::fhs_paths()
    }

    fn render(
        m: &PluginManifest,
        paths: &Paths,
        granted: &BTreeSet<String>,
        guard: bool,
    ) -> String {
        render_systemd(
            &build_unit_spec(m, paths, "drone", granted, guard)
                .unwrap()
                .unwrap(),
        )
    }

    fn subprocess_manifest() -> PluginManifest {
        PluginManifest::from_yaml_text(
            "id: com.example.thermal-lepton\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  entrypoint: agent/py/thermal.py\n  resources:\n    max_ram_mb: 128\n    max_cpu_percent: 40\n    max_pids: 16\n",
        )
        .unwrap()
    }

    /// The whole main unit, pinned: the text a running node's units were
    /// rendered with, plus the socket-bind deny every plugin unit now carries
    /// and the credential files under `/etc/ados` every unit hides.
    const RUST_UNIT_GOLDEN: &str = "\
[Unit]
Description=ADOS plugin com.example.rustplug
After=ados-supervisor.service
PartOf=ados-supervisor.service
# No start rate limit: a plugin whose host socket is not up yet must keep
# retrying rather than land in a failed state an operator has to clear by hand.
StartLimitIntervalSec=0

[Service]
Slice=ados-plugins.slice
Type=simple
Environment=ADOS_PLUGIN_SOCKET=/run/ados/plugins/com.example.rustplug/host.sock
Environment=ADOS_NODE_PROFILE=drone
EnvironmentFile=-/run/ados/plugins/com.example.rustplug.token.env
BindReadOnlyPaths=/run/ados/plugins/com.example.rustplug
ExecStart=/var/ados/plugins/com.example.rustplug/agent/bin/com.example.rustplug com.example.rustplug --socket /run/ados/plugins/com.example.rustplug/host.sock
Restart=on-failure
RestartSec=2s
MemoryMax=96M
CPUQuota=25%
TasksMax=12
StandardOutput=append:/var/log/ados/plugins/com-example-rustplug.log
StandardError=append:/var/log/ados/plugins/com-example-rustplug.log
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
DevicePolicy=closed
DeviceAllow=char-i2c rw
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK
SocketBindDeny=any
TemporaryFileSystem=/run/ados:ro
BindReadOnlyPaths=-/run/ados/logd.sock
ReadWritePaths=/var/ados/plugin-data /var/log/ados/plugins /srv /mnt /media /boot
ProtectHome=read-only
InaccessiblePaths=-/etc/ados/secrets -/etc/ados/plugin-keys -/etc/ados/pairing.json -/etc/ados/config.yaml -/etc/ados/mcp-token.json -/etc/ados/dashboard-pin.json -/etc/ados/wfb -/etc/ados/mesh -/etc/ados/ap-passphrase -/etc/ados/hostapd-gs.conf -/etc/ados/model-registry-auth.json -/etc/ados/plugin-config.json

[Install]
WantedBy=ados-supervisor.service
";

    #[test]
    fn the_main_unit_text_is_pinned() {
        let m = PluginManifest::from_yaml_text(
            "id: com.example.rustplug\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  entrypoint: agent/bin/com.example.rustplug\n  runtime: rust\n",
        )
        .unwrap();
        let granted: BTreeSet<String> = ["hardware.i2c", "network.outbound", "filesystem.host"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(render(&m, &fhs(), &granted, true), RUST_UNIT_GOLDEN);
    }

    #[test]
    fn the_log_path_stays_where_the_janitor_looks_for_it() {
        // systemd cannot cap an `append:` file, so the only thing bounding these
        // is the supervisor's janitor, which finds them by this directory and
        // this suffix. If either moves without the janitor moving with it, the
        // logs go unbounded again and nothing else here would notice.
        let path = log_path_for(Path::new(PLUGIN_LOG_DIR), "com.example.thermal-lepton");
        assert!(
            path.starts_with(PLUGIN_LOG_DIR),
            "plugin logs must stay in the directory the janitor sweeps: {path:?}"
        );
        assert!(
            path.to_string_lossy().ends_with(PLUGIN_LOG_SUFFIX),
            "plugin logs must keep the suffix the janitor selects on: {path:?}"
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
        let unit = render(&subprocess_manifest(), &fhs(), &BTreeSet::new(), false);
        assert!(unit.contains("Slice=ados-plugins.slice"));
        // Python runtime (default): the shared runner takes the plugin id.
        assert!(unit.contains(
            "ExecStart=/opt/ados/venv/bin/ados-plugin-runner com.example.thermal-lepton"
        ));
        assert!(unit.contains("MemoryMax=128M"));
        assert!(unit.contains("CPUQuota=40%"));
        assert!(unit.contains("TasksMax=16"));
        for flag in [
            "NoNewPrivileges=yes",
            "PrivateTmp=yes",
            "ProtectSystem=strict",
            "ProtectHome=yes",
            "LockPersonality=yes",
            "RestrictRealtime=yes",
            "RestrictSUIDSGID=yes",
            "SocketBindDeny=any",
        ] {
            assert!(unit.contains(flag), "missing {flag}");
        }
        assert!(
            !unit.contains("SocketBindAllow="),
            "the main unit never listens"
        );
        // Log append path uses the sanitized id.
        assert!(unit.contains(
            "StandardOutput=append:/var/log/ados/plugins/com-example-thermal-lepton.log"
        ));
    }

    #[test]
    fn the_layout_follows_the_paths_it_is_built_against() {
        // A per-user install (the macOS workstation) moves every host path;
        // nothing in the unit may still point at the FHS layout.
        let root = Path::new("/Users/op/.ados");
        let paths = Paths {
            install_dir: root.join("plugins"),
            unit_dir: root.join("agents"),
            state_path: root.join("plugin-state/plugin-state.json"),
            log_dir: root.join("log/plugins"),
            control_dir: root.join("run/plugin-host"),
            loopback_guard_state: root.join("run/plugin-loopback-guard.json"),
            socket_dir: root.join("run/plugins"),
            token_secret: root.join("secrets/plugin-token-secret"),
            runner: root.join("venv/bin/ados-plugin-runner"),
            run_dir: root.join("run"),
        };
        let unit = render(&subprocess_manifest(), &paths, &BTreeSet::new(), false);
        assert!(unit.contains(
            "ExecStart=/Users/op/.ados/venv/bin/ados-plugin-runner com.example.thermal-lepton"
        ));
        assert!(unit.contains(
            "Environment=ADOS_PLUGIN_SOCKET=/Users/op/.ados/run/plugins/com.example.thermal-lepton/host.sock"
        ));
        assert!(unit.contains(
            "EnvironmentFile=-/Users/op/.ados/run/plugins/com.example.thermal-lepton.token.env"
        ));
        assert!(unit.contains(
            "StandardOutput=append:/Users/op/.ados/log/plugins/com-example-thermal-lepton.log"
        ));
    }

    #[test]
    fn a_bin_entrypoint_resolves_to_this_hosts_binary_or_is_refused() {
        let arch_os = host_arch_os();
        let with = |arch: &str| {
            PluginManifest::from_yaml_text(&format!(
                "id: com.example.multi\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  entrypoint: \"bin:multi-link\"\n  runtime: rust\n  binaries:\n    multi-link:\n      {arch}: bin/{arch}/multi-link\n"
            ))
            .unwrap()
        };
        let unit = render(&with(&arch_os), &fhs(), &BTreeSet::new(), false);
        assert!(unit.contains(&format!(
            "ExecStart=/var/ados/plugins/com.example.multi/bin/{arch_os}/multi-link com.example.multi --socket"
        )));

        let other = if arch_os == "riscv64-linux" {
            "aarch64-linux"
        } else {
            "riscv64-linux"
        };
        let err =
            build_unit_spec(&with(other), &fhs(), "drone", &BTreeSet::new(), false).unwrap_err();
        assert_eq!(err.0, format!("incompatible: no binary for {arch_os}"));
    }

    #[test]
    fn an_http_plugin_gets_its_http_dir_bound_writable() {
        let m = PluginManifest::from_yaml_text(
            "id: com.example.web\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  entrypoint: agent/py/x.py\n  http: true\n",
        )
        .unwrap();
        let unit = render(&m, &fhs(), &BTreeSet::new(), false);
        assert!(
            unit.contains("BindPaths=/run/ados/plugin-http/com.example.web\n"),
            "{unit}"
        );
        assert!(unit.contains(
            "Environment=ADOS_PLUGIN_HTTP_SOCKET=/run/ados/plugin-http/com.example.web/http.sock"
        ));
        let plain = render(&subprocess_manifest(), &fhs(), &BTreeSet::new(), false);
        assert!(!plain.contains("plugin-http"));
    }

    #[test]
    fn a_unit_reaches_its_own_socket_dir_and_no_other_plugins() {
        // Two plugins on one box. Each unit hides the whole run dir and binds
        // back exactly one plugin directory, its own, which holds the socket
        // its runner is pointed at.
        let unit_for = |id: &str| {
            let m = PluginManifest::from_yaml_text(&format!(
                "id: {id}\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  entrypoint: agent/py/p.py\n"
            ))
            .unwrap();
            render(&m, &fhs(), &BTreeSet::new(), false)
        };
        for (id, other) in [
            ("com.example.a", "com.example.b"),
            ("com.example.b", "com.example.a"),
        ] {
            let unit = unit_for(id);
            assert!(unit.contains("TemporaryFileSystem=/run/ados:ro"), "{unit}");
            let binds: Vec<&str> = unit
                .lines()
                .filter_map(|l| l.strip_prefix("BindReadOnlyPaths="))
                .filter(|p| p.contains("/run/ados/plugins"))
                .collect();
            assert_eq!(binds, vec![format!("/run/ados/plugins/{id}")], "{unit}");
            assert!(
                !unit.contains(&format!("/run/ados/plugins/{other}")),
                "{unit}"
            );
        }
    }

    #[test]
    fn inprocess_and_gcs_only_build_no_unit() {
        let inproc = PluginManifest::from_yaml_text(
            "id: com.altnautica.builtin\nversion: 0.1.0\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  entrypoint: pkg:Class\n  isolation: inprocess\n",
        )
        .unwrap();
        assert!(
            build_unit_spec(&inproc, &fhs(), "drone", &BTreeSet::new(), false)
                .unwrap()
                .is_none()
        );

        let gcs_only = PluginManifest::from_yaml_text(
            "id: com.example.panel\nversion: 0.1.0\ncompatibility:\n  ados_version: \">=0.1.0\"\ngcs:\n  entrypoint: gcs/dist/index.js\n",
        )
        .unwrap();
        assert!(
            build_unit_spec(&gcs_only, &fhs(), "drone", &BTreeSet::new(), false)
                .unwrap()
                .is_none()
        );
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
            let err = build_unit_spec(&m, &fhs(), "drone", &BTreeSet::new(), false)
                .expect_err(&format!("{id:?} / {entrypoint:?} must be refused"));
            assert!(err.0.contains("refusing to render"), "{}", err.0);
        }
        // An install dir with whitespace is refused on the rust ExecStart too.
        let m = unvalidated_rust_manifest("com.example.x", "bin/x");
        let mut spaced = fhs();
        spaced.install_dir = PathBuf::from("/var/ados/my plugins");
        assert!(build_unit_spec(&m, &spaced, "drone", &BTreeSet::new(), false).is_err());
        // The same manifest builds under a clean install dir.
        assert!(
            build_unit_spec(&m, &fhs(), "drone", &BTreeSet::new(), false)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn slice_content_has_accounting_block() {
        let s = PLUGIN_SLICE_CONTENT;
        assert!(s.contains("[Slice]"));
        assert!(s.contains("CPUAccounting=yes"));
        assert!(s.contains("MemoryAccounting=yes"));
        assert!(s.contains("TasksAccounting=yes"));
        assert!(s.contains("IOAccounting=yes"));
    }
}
