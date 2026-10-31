//! The systemd service backend and its unit rendering.

use super::*;

/// Render a [`UnitSpec`] as a systemd service unit.
///
/// Every generated plugin unit shares this text: the plugin slice, no start
/// rate limit, the ordering on the plugin host (whose guard-load pre-start
/// runs before the plugin's own process), the runner environment and token
/// credential, the binds, the resource envelope, the append logs, the plugin's
/// own user in the plugin group, the fixed hardening and exec allowlist, then
/// the capability sandbox (the part a grant or revoke changes).
pub fn render_systemd(spec: &UnitSpec) -> String {
    let mut service: Vec<String> = vec![
        format!("Slice={PLUGIN_SLICE_NAME}"),
        "Type=simple".to_string(),
    ];
    if let Some(dir) = &spec.working_dir {
        service.push(format!("WorkingDirectory={}", dir.display()));
    }
    for (key, value) in &spec.env {
        service.push(format!("Environment={key}={value}"));
    }
    if let Some(file) = &spec.credential {
        service.push(format!(
            "LoadCredential={TOKEN_CREDENTIAL_NAME}:{}",
            file.display()
        ));
    }
    for path in &spec.bind_read_only {
        service.push(format!("BindReadOnlyPaths={}", path.display()));
    }
    for path in &spec.bind_read_write {
        service.push(format!("BindPaths={}", path.display()));
    }
    if let Some(loader) = &spec.guard_loader {
        // `+` runs it with full privileges, free of the unit's privilege and
        // path restrictions, though still inside the unit's temporary
        // filesystems and bind mounts; a non-zero exit fails the start, so
        // the plugin never runs unguarded.
        service.push(format!("ExecStartPre=+{} guard-load", loader.display()));
    }
    let exec: Vec<String> = spec.argv.iter().map(|w| exec_word(w)).collect();
    service.push(format!("ExecStart={}", exec.join(" ")));
    let log = spec.log_path.display();
    let res = &spec.resources;
    service.extend([
        format!("Restart={}", spec.restart),
        "RestartSec=2s".to_string(),
        format!("MemoryMax={}M", res.max_ram_mb),
        format!("CPUQuota={}%", res.max_cpu_percent),
        format!("TasksMax={}", res.max_pids),
        format!("StandardOutput=append:{log}"),
        format!("StandardError=append:{log}"),
        format!("User={}", spec.user),
        format!("Group={PLUGIN_GROUP}"),
    ]);
    service.extend(HARDENING_DIRECTIVES.iter().map(|s| s.to_string()));
    service.extend(exec_allowlist(&spec.exec_paths));
    service.push("# ---- capability sandbox (re-rendered on every grant/revoke) ----".to_string());
    service.extend(spec.sandbox_directives.iter().cloned());
    format!(
        "\
[Unit]
Description={description}
After=ados-supervisor.service {PLUGIN_HOST_UNIT}
Requires={PLUGIN_HOST_UNIT}
PartOf=ados-supervisor.service
# No start rate limit: a plugin whose host socket is not up yet must keep
# retrying rather than land in a failed state an operator has to clear by hand.
StartLimitIntervalSec=0

[Service]
{service}

[Install]
WantedBy=ados-supervisor.service
",
        description = spec.description,
        service = service.join("\n"),
    )
}

/// The unit every plugin unit requires and orders after: its `guard-load`
/// pre-start and its socket server both live in the plugin host.
pub const PLUGIN_HOST_UNIT: &str = "ados-plugin-host.service";

/// `NoExecPaths=/` plus the allowlist, so the plugin can execute (and map
/// executable code from) only its own tree, its runtime and the shared
/// libraries, never `/bin/sh` or another binary it was not approved to run.
/// Empty when the spec carries no allowlist.
pub fn exec_allowlist(exec_paths: &[String]) -> Vec<String> {
    if exec_paths.is_empty() {
        return Vec::new();
    }
    vec![
        "NoExecPaths=/".to_string(),
        format!("ExecPaths={}", exec_paths.join(" ")),
    ]
}

impl SystemdBackend {
    pub fn new(unit_dir: &Path) -> Self {
        SystemdBackend {
            unit_dir: unit_dir.to_path_buf(),
            run: bounded_runner(),
        }
    }

    /// Substitute the command runner (tests record the argv instead of
    /// invoking systemd).
    pub fn with_runner(mut self, run: CommandRunner) -> Self {
        self.run = run;
        self
    }

    fn systemctl(&self, args: &[&str]) -> Result<(), SupervisorError> {
        (self.run)("systemctl", args)
    }

    /// The shared plugin slice every unit runs in. Written once.
    fn ensure_slice(&self) -> Result<(), SupervisorError> {
        let path = self.unit_dir.join(PLUGIN_SLICE_NAME);
        if path.exists() {
            return Ok(());
        }
        write_if_changed(&path, PLUGIN_SLICE_CONTENT)?;
        self.systemctl(&["daemon-reload"])
    }
}

impl ServiceBackend for SystemdBackend {
    fn install(&self, name: &str, spec: &UnitSpec) -> Result<bool, SupervisorError> {
        self.ensure_slice()?;
        let changed = write_if_changed(&self.unit_dir.join(name), &render_systemd(spec))?;
        if changed {
            self.systemctl(&["daemon-reload"])?;
        }
        Ok(changed)
    }

    fn uninstall(&self, name: &str) -> Result<(), SupervisorError> {
        let path = self.unit_dir.join(name);
        match std::fs::remove_file(&path) {
            Ok(()) => self.systemctl(&["daemon-reload"]),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(SupervisorError(format!("remove {}: {e}", path.display()))),
        }
    }

    fn enable_start(&self, name: &str) -> Result<(), SupervisorError> {
        self.systemctl(&["enable", name])?;
        self.systemctl(&["start", name])
    }

    fn stop_disable(&self, name: &str) -> Result<(), SupervisorError> {
        self.systemctl(&["stop", name])?;
        self.systemctl(&["disable", name])
    }

    fn restart(&self, name: &str) -> Result<(), SupervisorError> {
        self.systemctl(&["restart", name])
    }

    fn is_active(&self, name: &str) -> Result<bool, SupervisorError> {
        Ok(self.systemctl(&["is-active", "--quiet", name]).is_ok())
    }

    /// Run the probe as a transient unit with exactly what the plugin's
    /// services get: the plugin's own user in the plugin group, the hardening,
    /// the exec allowlist, the resource envelope, the capability sandbox and
    /// the plugin slice. The argv is passed through as separate words; no shell
    /// sees it.
    fn probe(&self, probe: &ProbeSpec) -> Result<(), SupervisorError> {
        let argv = systemd_run_argv(probe);
        let args: Vec<&str> = argv.iter().map(String::as_str).collect();
        run_bounded(
            "systemd-run",
            &args,
            None,
            PROBE_TIMEOUT + Duration::from_secs(5),
        )
        .map(|_| ())
    }

    fn enforces_sandbox(&self) -> bool {
        true
    }
}

/// The `systemd-run` arguments that run one readiness probe as a transient,
/// sandboxed unit.
pub fn systemd_run_argv(probe: &ProbeSpec) -> Vec<String> {
    let mut out: Vec<String> = ["--quiet", "--wait", "--pipe", "--collect"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    out.push(format!("--uid={}", probe.user));
    out.push(format!("--gid={PLUGIN_GROUP}"));
    out.push(format!("--slice={PLUGIN_SLICE_NAME}"));
    out.push(format!(
        "--working-directory={}",
        probe.working_dir.display()
    ));
    let res = &probe.resources;
    let mut properties: Vec<String> = HARDENING_DIRECTIVES.iter().map(|s| s.to_string()).collect();
    properties.extend(exec_allowlist(&probe.exec_paths));
    properties.push(format!("MemoryMax={}M", res.max_ram_mb));
    properties.push(format!("CPUQuota={}%", res.max_cpu_percent));
    properties.push(format!("TasksMax={}", res.max_pids));
    properties.push(format!("RuntimeMaxSec={}", PROBE_TIMEOUT.as_secs()));
    properties.extend(probe.sandbox_directives.iter().cloned());
    out.extend(properties.into_iter().map(|p| format!("--property={p}")));
    out.push("--".to_string());
    out.extend(probe.argv.iter().cloned());
    out
}
