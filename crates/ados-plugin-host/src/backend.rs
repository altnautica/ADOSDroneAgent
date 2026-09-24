//! Service backends: how the lifecycle controller installs, starts, stops and
//! probes a plugin's processes on the host's service manager.
//!
//! The controller describes each process as a [`UnitSpec`] (argv, environment,
//! token file, binds, log, resource envelope, capability sandbox) and hands it
//! to a [`ServiceBackend`] by unit name. Two backends ship:
//!
//! * [`SystemdBackend`] (Linux) renders a systemd unit with [`render_systemd`]
//!   into the unit dir and drives `systemctl`. It enforces the sandbox.
//! * [`LaunchdBackend`] (macOS) renders a launchd property list into the
//!   per-user LaunchAgents dir and drives `launchctl` in the operator's GUI
//!   domain. launchd has no device policy, address-family filter or mount
//!   namespace, so it enforces no sandbox; the controller therefore installs
//!   only first-party plugins on it.
//!
//! [`RecordingBackend`] keeps the rendered units in memory and records every
//! verb, so the controller's tests never touch a service manager.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use std::time::Duration;

use crate::errors::SupervisorError;
use crate::manifest::ResourceLimits;
use crate::services::exec_word;
use crate::systemd::{HARDENING_DIRECTIVES, PLUGIN_SLICE_CONTENT, PLUGIN_SLICE_NAME};

/// How long one service-manager call may take before it is abandoned. Killing
/// the client does not cancel a job the manager already queued; it only frees
/// the caller, which otherwise held the controller (and every mutex around it)
/// for as long as a wedged plugin took to stop.
pub const SERVICE_CALL_TIMEOUT: Duration = Duration::from_secs(15);

/// One process the service manager runs for a plugin: its main half or one of
/// its declared services.
#[derive(Debug, Clone)]
pub struct UnitSpec {
    /// Human-readable description (`ADOS plugin <id>`).
    pub description: String,
    /// The exec line, program first. Each word is a single validated token.
    pub argv: Vec<String>,
    /// Working directory, when the command is relative to the plugin tree.
    pub working_dir: Option<PathBuf>,
    /// Static environment.
    pub env: Vec<(String, String)>,
    /// The owner-only `KEY=VALUE` file carrying the capability token, read at
    /// every start. Optional at start: the runner waits for it.
    pub env_file: Option<PathBuf>,
    /// Paths bound read-only into the sandbox (the plugin's socket dir).
    pub bind_read_only: Vec<PathBuf>,
    /// Paths bound read-write into the sandbox (the plugin's HTTP dir).
    pub bind_read_write: Vec<PathBuf>,
    /// Where stdout and stderr append.
    pub log_path: PathBuf,
    /// `on-failure`, `always` or `no`.
    pub restart: String,
    /// The resource envelope.
    pub resources: ResourceLimits,
    /// The capability sandbox lines ([`crate::sandbox::sandbox_directives`]).
    pub sandbox_directives: Vec<String>,
}

/// A readiness probe command, run with what the plugin's services get.
#[derive(Debug, Clone)]
pub struct ProbeSpec {
    pub argv: Vec<String>,
    pub working_dir: PathBuf,
    pub resources: ResourceLimits,
    pub sandbox_directives: Vec<String>,
}

/// How long one command readiness probe may run.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// The host service manager, addressed by unit name
/// (`ados-plugin-<id>.service`, `ados-plugin-<id>-<service>.service`).
pub trait ServiceBackend: Send + Sync {
    /// Write the unit for `spec`. Returns whether the on-disk definition
    /// changed, so a caller restarts a running unit only when it must.
    fn install(&self, name: &str, spec: &UnitSpec) -> Result<bool, SupervisorError>;
    /// Unload and delete the unit. A unit that is not there is not an error.
    fn uninstall(&self, name: &str) -> Result<(), SupervisorError>;
    /// Enable the unit at boot and start it now.
    fn enable_start(&self, name: &str) -> Result<(), SupervisorError>;
    /// Stop the unit and disable it at boot.
    fn stop_disable(&self, name: &str) -> Result<(), SupervisorError>;
    /// Restart the unit so it runs the current definition.
    fn restart(&self, name: &str) -> Result<(), SupervisorError>;
    /// Whether the unit is running.
    fn is_active(&self, name: &str) -> Result<bool, SupervisorError>;
    /// Run a readiness probe to completion; `Ok` on exit 0.
    fn probe(&self, probe: &ProbeSpec) -> Result<(), SupervisorError>;
    /// Whether this backend applies the capability sandbox the unit spec
    /// carries. A backend that does not may run first-party plugins only.
    fn enforces_sandbox(&self) -> bool;
}

/// The backend for this host: launchd on macOS, systemd elsewhere.
pub fn default_backend(unit_dir: &Path) -> Arc<dyn ServiceBackend> {
    if cfg!(target_os = "macos") {
        Arc::new(LaunchdBackend::new(unit_dir))
    } else {
        Arc::new(SystemdBackend::new(unit_dir))
    }
}

/// Render a [`UnitSpec`] as a systemd service unit.
///
/// Every generated plugin unit shares this text: the plugin slice, no start
/// rate limit, the runner environment and token file, the binds, the resource
/// envelope, the append logs, the `ados` user, the fixed hardening, then the
/// capability sandbox (the part a grant or revoke changes).
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
    if let Some(file) = &spec.env_file {
        service.push(format!("EnvironmentFile=-{}", file.display()));
    }
    for path in &spec.bind_read_only {
        service.push(format!("BindReadOnlyPaths={}", path.display()));
    }
    for path in &spec.bind_read_write {
        service.push(format!("BindPaths={}", path.display()));
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
        "User=ados".to_string(),
        "Group=ados".to_string(),
    ]);
    service.extend(HARDENING_DIRECTIVES.iter().map(|s| s.to_string()));
    service.push("# ---- capability sandbox (re-rendered on every grant/revoke) ----".to_string());
    service.extend(spec.sandbox_directives.iter().cloned());
    format!(
        "\
[Unit]
Description={description}
After=ados-supervisor.service
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

/// Write `text` to `path` when it differs from what is there. Returns whether
/// it wrote.
fn write_if_changed(path: &Path, text: &str) -> Result<bool, SupervisorError> {
    if std::fs::read_to_string(path).is_ok_and(|current| current == text) {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| SupervisorError(format!("create {}: {e}", parent.display())))?;
    }
    std::fs::write(path, text.as_bytes())
        .map_err(|e| SupervisorError(format!("write {}: {e}", path.display())))?;
    Ok(true)
}

/// Run `program args...` in `cwd`, waiting at most `deadline` for it to exit.
/// Returns its stdout on exit 0. A child still running at the deadline is
/// killed and reported as a timeout; a non-zero exit carries its stderr.
pub(crate) fn run_bounded(
    program: &str,
    args: &[&str],
    cwd: Option<&Path>,
    deadline: Duration,
) -> Result<String, SupervisorError> {
    use std::io::Read;
    let label = format!("{program} {}", args.join(" "));
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    let mut child = command.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            SupervisorError(format!("{program} not found on this host"))
        } else {
            SupervisorError(format!("{label} failed to spawn: {e}"))
        }
    })?;
    // Drain both pipes on their own threads so a chatty child can never block
    // on a full pipe while this thread waits for it to exit.
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        pipe.map(|mut pipe| {
            std::thread::spawn(move || {
                let mut text = String::new();
                let _ = pipe.read_to_string(&mut text);
                text
            })
        })
    };
    let stdout = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let stderr = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(SupervisorError(format!(
                    "{label} timed out after {}s",
                    deadline.as_secs_f64()
                )));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(SupervisorError(format!("{label} wait failed: {e}"))),
        }
    };
    let out = stdout.and_then(|h| h.join().ok()).unwrap_or_default();
    if !status.success() {
        let text = stderr.and_then(|h| h.join().ok()).unwrap_or_default();
        return Err(SupervisorError(format!("{label} failed: {}", text.trim())));
    }
    Ok(out)
}

/// Runs one external command for a backend: `(program, args)`. The production
/// runner is [`run_bounded`]; a test substitutes a recorder.
pub type CommandRunner = Arc<dyn Fn(&str, &[&str]) -> Result<(), SupervisorError> + Send + Sync>;

fn bounded_runner() -> CommandRunner {
    Arc::new(|program, args| run_bounded(program, args, None, SERVICE_CALL_TIMEOUT).map(|_| ()))
}

// ---------------------------------------------------------------------------
// systemd
// ---------------------------------------------------------------------------

/// systemd: unit files under the unit dir, `systemctl` verbs, sandboxed
/// readiness probes through `systemd-run`.
pub struct SystemdBackend {
    unit_dir: PathBuf,
    run: CommandRunner,
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
    /// services get: the `ados` user, the hardening, the resource envelope,
    /// the capability sandbox and the plugin slice. The argv is passed through
    /// as separate words; no shell sees it.
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
    let mut out: Vec<String> = [
        "--quiet",
        "--wait",
        "--pipe",
        "--collect",
        "--uid=ados",
        "--gid=ados",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    out.push(format!("--slice={PLUGIN_SLICE_NAME}"));
    out.push(format!(
        "--working-directory={}",
        probe.working_dir.display()
    ));
    let res = &probe.resources;
    let mut properties: Vec<String> = HARDENING_DIRECTIVES.iter().map(|s| s.to_string()).collect();
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

// ---------------------------------------------------------------------------
// launchd
// ---------------------------------------------------------------------------

/// Reads the unit's token file into the environment, enters its working
/// directory, then execs the program: launchd has no `EnvironmentFile=` or
/// `WorkingDirectory=` of its own that tracks a file rewritten at every token
/// rotation. Positional arguments: `$1` working dir (may be empty), `$2` env
/// file (may be empty), then the argv. Values are taken verbatim after the
/// first `=`; nothing is evaluated.
const LAUNCHD_ENTRY_SCRIPT: &str = "wd=\"$1\"; envf=\"$2\"; shift 2; \
if [ -n \"$wd\" ]; then cd \"$wd\" || exit 78; fi; \
if [ -n \"$envf\" ] && [ -r \"$envf\" ]; then \
while IFS= read -r l || [ -n \"$l\" ]; do \
case \"$l\" in *=*) export \"${l%%=*}=${l#*=}\";; esac; \
done < \"$envf\"; fi; \
exec \"$@\"";

/// launchd: property lists under the per-user LaunchAgents dir, `launchctl`
/// verbs in the operator's GUI domain. Enforces no sandbox.
pub struct LaunchdBackend {
    unit_dir: PathBuf,
    run: CommandRunner,
}

impl LaunchdBackend {
    pub fn new(unit_dir: &Path) -> Self {
        LaunchdBackend {
            unit_dir: unit_dir.to_path_buf(),
            run: bounded_runner(),
        }
    }

    /// Substitute the command runner (tests record the argv).
    pub fn with_runner(mut self, run: CommandRunner) -> Self {
        self.run = run;
        self
    }

    /// `co.ados.plugin.<unit token>` for `ados-plugin-<token>.service`.
    pub fn label_for(name: &str) -> String {
        let base = name.strip_suffix(".service").unwrap_or(name);
        let token = base
            .strip_prefix(crate::systemd::PLUGIN_UNIT_PREFIX)
            .unwrap_or(base);
        format!("co.ados.plugin.{token}")
    }

    fn plist_path(&self, name: &str) -> PathBuf {
        self.unit_dir
            .join(format!("{}.plist", Self::label_for(name)))
    }

    fn domain() -> String {
        format!("gui/{}", ados_protocol::launchd::current_uid())
    }

    fn target(name: &str) -> String {
        format!("{}/{}", Self::domain(), Self::label_for(name))
    }

    fn launchctl(&self, args: &[&str]) -> Result<(), SupervisorError> {
        (self.run)("launchctl", args)
    }

    /// Load the job into the domain; a job already loaded is not an error.
    fn bootstrap(&self, name: &str) -> Result<(), SupervisorError> {
        let plist = self.plist_path(name);
        let plist = plist.to_string_lossy();
        match self.launchctl(&["bootstrap", &Self::domain(), &plist]) {
            Ok(()) => Ok(()),
            Err(e) if self.loaded(name) => {
                tracing::debug!(unit = name, error = %e, "launchd job already loaded");
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    fn loaded(&self, name: &str) -> bool {
        self.launchctl(&["print", &Self::target(name)]).is_ok()
    }

    /// Unload the job; a job that is not loaded is not an error.
    fn bootout(&self, name: &str) {
        if let Err(e) = self.launchctl(&["bootout", &Self::target(name)]) {
            tracing::debug!(unit = name, error = %e, "launchd bootout (job not loaded)");
        }
    }
}

/// Render a [`UnitSpec`] as a launchd property list. The program is
/// `/bin/sh` running [`LAUNCHD_ENTRY_SCRIPT`], which reads the token file and
/// enters the working directory before exec'ing the unit's argv.
pub fn render_launchd(label: &str, spec: &UnitSpec) -> String {
    let mut args: Vec<String> = vec![
        "-c".to_string(),
        LAUNCHD_ENTRY_SCRIPT.to_string(),
        "ados-plugin".to_string(),
        spec.working_dir
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        spec.env_file
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
    ];
    args.extend(spec.argv.iter().cloned());
    let log = spec.log_path.display().to_string();
    ados_protocol::launchd::render_plist(
        label,
        "/bin/sh",
        &args,
        &spec.env,
        spec.restart != "no",
        ados_protocol::launchd::PlistLogPaths {
            stdout: Some(&log),
            stderr: Some(&log),
        },
    )
}

impl ServiceBackend for LaunchdBackend {
    fn install(&self, name: &str, spec: &UnitSpec) -> Result<bool, SupervisorError> {
        write_if_changed(
            &self.plist_path(name),
            &render_launchd(&Self::label_for(name), spec),
        )
    }

    fn uninstall(&self, name: &str) -> Result<(), SupervisorError> {
        self.bootout(name);
        let path = self.plist_path(name);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(SupervisorError(format!("remove {}: {e}", path.display()))),
        }
    }

    fn enable_start(&self, name: &str) -> Result<(), SupervisorError> {
        self.launchctl(&["enable", &Self::target(name)])?;
        self.bootstrap(name)?;
        self.launchctl(&["kickstart", &Self::target(name)])
    }

    fn stop_disable(&self, name: &str) -> Result<(), SupervisorError> {
        self.bootout(name);
        self.launchctl(&["disable", &Self::target(name)])
    }

    /// A changed plist only takes effect on a fresh load, so a restart is an
    /// unload, a load and a start.
    fn restart(&self, name: &str) -> Result<(), SupervisorError> {
        self.bootout(name);
        self.bootstrap(name)?;
        self.launchctl(&["kickstart", "-k", &Self::target(name)])
    }

    fn is_active(&self, name: &str) -> Result<bool, SupervisorError> {
        match run_bounded(
            "launchctl",
            &["print", &Self::target(name)],
            None,
            SERVICE_CALL_TIMEOUT,
        ) {
            Ok(out) => Ok(out.lines().any(|l| l.trim() == "state = running")),
            Err(_) => Ok(false),
        }
    }

    /// No sandbox to run the probe in: the probe runs as the operator, in the
    /// plugin's install dir, bounded by the probe timeout.
    fn probe(&self, probe: &ProbeSpec) -> Result<(), SupervisorError> {
        let Some((program, rest)) = probe.argv.split_first() else {
            return Err(SupervisorError("empty readiness probe".to_string()));
        };
        let args: Vec<&str> = rest.iter().map(String::as_str).collect();
        run_bounded(program, &args, Some(&probe.working_dir), PROBE_TIMEOUT).map(|_| ())
    }

    fn enforces_sandbox(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// recording
// ---------------------------------------------------------------------------

/// What [`RecordingBackend::on_enable_start`] runs at each start.
type StartHook = Box<dyn Fn(&str) + Send + Sync>;

/// A backend that touches no service manager: it keeps each installed unit's
/// rendered systemd text in memory and records every verb as `(verb, name)`.
/// Lives in the crate (not `#[cfg(test)]`) so dependent crates' tests can use
/// it. Every unit reads active and every probe passes.
pub struct RecordingBackend {
    calls: Mutex<Vec<(String, String)>>,
    units: Mutex<BTreeMap<String, String>>,
    sandbox: bool,
    on_enable_start: Option<StartHook>,
}

impl Default for RecordingBackend {
    fn default() -> Self {
        RecordingBackend {
            calls: Mutex::new(Vec::new()),
            units: Mutex::new(BTreeMap::new()),
            sandbox: true,
            on_enable_start: None,
        }
    }
}

impl RecordingBackend {
    /// A recorder that reports no sandbox, as launchd does.
    pub fn without_sandbox() -> Self {
        RecordingBackend {
            sandbox: false,
            ..RecordingBackend::default()
        }
    }

    /// Run `hook` with the unit name at every `enable_start`, the moment a
    /// real service manager would exec the unit, so a test can check what
    /// exists by then.
    pub fn on_enable_start(mut self, hook: impl Fn(&str) + Send + Sync + 'static) -> Self {
        self.on_enable_start = Some(Box::new(hook));
        self
    }

    /// Every `(verb, unit name)` recorded, in order.
    pub fn calls(&self) -> Vec<(String, String)> {
        self.calls.lock().clone()
    }

    /// Whether `verb` was recorded for `name`.
    pub fn called(&self, verb: &str, name: &str) -> bool {
        self.calls().iter().any(|(v, n)| v == verb && n == name)
    }

    /// Forget the recorded calls.
    pub fn clear_calls(&self) {
        self.calls.lock().clear();
    }

    /// The rendered systemd text of an installed unit.
    pub fn unit(&self, name: &str) -> Option<String> {
        self.units.lock().get(name).cloned()
    }

    fn record(&self, verb: &str, name: &str) {
        self.calls.lock().push((verb.to_string(), name.to_string()));
    }
}

impl ServiceBackend for RecordingBackend {
    fn install(&self, name: &str, spec: &UnitSpec) -> Result<bool, SupervisorError> {
        self.record("install", name);
        let text = render_systemd(spec);
        let previous = self.units.lock().insert(name.to_string(), text.clone());
        Ok(previous.as_ref() != Some(&text))
    }

    fn uninstall(&self, name: &str) -> Result<(), SupervisorError> {
        self.record("uninstall", name);
        self.units.lock().remove(name);
        Ok(())
    }

    fn enable_start(&self, name: &str) -> Result<(), SupervisorError> {
        self.record("enable_start", name);
        if let Some(hook) = &self.on_enable_start {
            hook(name);
        }
        Ok(())
    }

    fn stop_disable(&self, name: &str) -> Result<(), SupervisorError> {
        self.record("stop_disable", name);
        Ok(())
    }

    fn restart(&self, name: &str) -> Result<(), SupervisorError> {
        self.record("restart", name);
        Ok(())
    }

    fn is_active(&self, name: &str) -> Result<bool, SupervisorError> {
        self.record("is_active", name);
        Ok(true)
    }

    fn probe(&self, _probe: &ProbeSpec) -> Result<(), SupervisorError> {
        self.record("probe", "");
        Ok(())
    }

    fn enforces_sandbox(&self) -> bool {
        self.sandbox
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> UnitSpec {
        UnitSpec {
            description: "ADOS plugin com.example.x".to_string(),
            argv: vec![
                "/var/ados/plugins/com.example.x/bin/x".to_string(),
                "com.example.x".to_string(),
            ],
            working_dir: None,
            env: vec![(
                "ADOS_PLUGIN_SOCKET".to_string(),
                "/run/ados/plugins/com.example.x/host.sock".to_string(),
            )],
            env_file: Some(PathBuf::from("/run/ados/plugins/com.example.x.token.env")),
            bind_read_only: vec![PathBuf::from("/run/ados/plugins/com.example.x")],
            bind_read_write: Vec::new(),
            log_path: PathBuf::from("/var/log/ados/plugins/com-example-x.log"),
            restart: "on-failure".to_string(),
            resources: ResourceLimits::default(),
            sandbox_directives: vec!["PrivateDevices=yes".to_string()],
        }
    }

    type Calls = Arc<Mutex<Vec<String>>>;

    fn recorder() -> (Calls, CommandRunner) {
        let calls: Calls = Arc::new(Mutex::new(Vec::new()));
        let sink = calls.clone();
        let run: CommandRunner = Arc::new(move |program, args| {
            sink.lock().push(format!("{program} {}", args.join(" ")));
            Ok(())
        });
        (calls, run)
    }

    #[test]
    fn systemd_writes_the_slice_and_unit_and_reloads_only_on_change() {
        let dir = tempfile::tempdir().unwrap();
        let (calls, run) = recorder();
        let backend = SystemdBackend::new(dir.path()).with_runner(run);
        let name = "ados-plugin-com-example-x.service";

        assert!(backend.install(name, &spec()).unwrap());
        assert!(dir.path().join(PLUGIN_SLICE_NAME).exists());
        assert_eq!(
            std::fs::read_to_string(dir.path().join(name)).unwrap(),
            render_systemd(&spec())
        );
        // Same spec again: nothing written, no reload.
        calls.lock().clear();
        assert!(!backend.install(name, &spec()).unwrap());
        assert!(calls.lock().is_empty());

        backend.enable_start(name).unwrap();
        backend.stop_disable(name).unwrap();
        backend.uninstall(name).unwrap();
        assert!(!dir.path().join(name).exists());
        assert_eq!(
            *calls.lock(),
            vec![
                format!("systemctl enable {name}"),
                format!("systemctl start {name}"),
                format!("systemctl stop {name}"),
                format!("systemctl disable {name}"),
                "systemctl daemon-reload".to_string(),
            ]
        );
    }

    #[test]
    fn launchd_renders_a_plist_that_reads_the_token_file_at_each_start() {
        let dir = tempfile::tempdir().unwrap();
        let (calls, run) = recorder();
        let backend = LaunchdBackend::new(dir.path()).with_runner(run);
        let name = "ados-plugin-com-example-x.service";
        assert_eq!(
            LaunchdBackend::label_for(name),
            "co.ados.plugin.com-example-x"
        );
        assert!(backend.install(name, &spec()).unwrap());
        let plist =
            std::fs::read_to_string(dir.path().join("co.ados.plugin.com-example-x.plist")).unwrap();
        assert!(plist.contains("<string>co.ados.plugin.com-example-x</string>"));
        assert!(plist.contains("<string>/bin/sh</string>"));
        assert!(plist.contains("<string>/run/ados/plugins/com.example.x.token.env</string>"));
        assert!(plist.contains("<key>ADOS_PLUGIN_SOCKET</key>"));
        assert!(plist.contains(
            "<key>StandardOutPath</key>\n\t<string>/var/log/ados/plugins/com-example-x.log"
        ));
        assert!(!backend.enforces_sandbox());

        backend.enable_start(name).unwrap();
        let recorded = calls.lock().clone();
        let target = format!(
            "gui/{}/co.ados.plugin.com-example-x",
            ados_protocol::launchd::current_uid()
        );
        assert_eq!(recorded[0], format!("launchctl enable {target}"));
        assert!(recorded[1].starts_with("launchctl bootstrap gui/"));
        assert!(recorded[1].ends_with("co.ados.plugin.com-example-x.plist"));
        assert_eq!(recorded[2], format!("launchctl kickstart {target}"));
    }

    #[test]
    fn the_launchd_entry_script_exports_the_token_file_verbatim() {
        // The token is `a|b=c` style text: nothing in it may be evaluated.
        let dir = tempfile::tempdir().unwrap();
        let envf = dir.path().join("x.token.env");
        std::fs::write(&envf, "ADOS_PLUGIN_TOKEN=a|b=c;$(false)\nADOS_X=1").unwrap();
        let out = std::process::Command::new("/bin/sh")
            .args([
                "-c",
                LAUNCHD_ENTRY_SCRIPT,
                "ados-plugin",
                &dir.path().display().to_string(),
                &envf.display().to_string(),
                "/bin/sh",
                "-c",
                "printf '%s|%s|%s' \"$ADOS_PLUGIN_TOKEN\" \"$ADOS_X\" \"$(pwd -P)\"",
            ])
            .output()
            .unwrap();
        let text = String::from_utf8(out.stdout).unwrap();
        let real = dir.path().canonicalize().unwrap();
        assert_eq!(text, format!("a|b=c;$(false)|1|{}", real.display()));
    }
}
