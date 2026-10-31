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
use crate::plugin_account::PLUGIN_GROUP;
use crate::services::exec_word;
use crate::systemd::{HARDENING_DIRECTIVES, PLUGIN_SLICE_CONTENT, PLUGIN_SLICE_NAME};
use crate::token_secret::TOKEN_CREDENTIAL_NAME;

mod launchd;
mod recording;
mod systemd;

pub use launchd::render_launchd;
pub use systemd::{exec_allowlist, render_systemd, systemd_run_argv, PLUGIN_HOST_UNIT};

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
    /// The plugin's own system user (see [`crate::plugin_account`]). Its primary
    /// group is always [`PLUGIN_GROUP`].
    pub user: String,
    /// The owner-only `KEY=VALUE` file carrying the capability token, loaded
    /// as a systemd credential at every start.
    pub credential: Option<PathBuf>,
    /// Paths bound read-only into the sandbox (the plugin's socket dir).
    pub bind_read_only: Vec<PathBuf>,
    /// Paths bound read-write into the sandbox (the plugin's HTTP dir).
    pub bind_read_write: Vec<PathBuf>,
    /// The only paths the unit may execute or map executable code from;
    /// everything else is mounted `noexec`. Optional entries carry a `-`.
    pub exec_paths: Vec<String>,
    /// The plugin-host binary whose `guard-load` runs as a privileged
    /// pre-start, so the loopback guard is loaded before the plugin runs and a
    /// failed load fails the unit.
    pub guard_loader: Option<PathBuf>,
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
    /// The plugin's own system user.
    pub user: String,
    pub resources: ResourceLimits,
    pub exec_paths: Vec<String>,
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

// ---------------------------------------------------------------------------
// launchd
// ---------------------------------------------------------------------------

/// launchd: property lists under the per-user LaunchAgents dir, `launchctl`
/// verbs in the operator's GUI domain. Enforces no sandbox.
pub struct LaunchdBackend {
    unit_dir: PathBuf,
    run: CommandRunner,
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

#[cfg(test)]
mod tests;
