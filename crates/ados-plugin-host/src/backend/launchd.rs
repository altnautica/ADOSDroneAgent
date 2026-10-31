//! The launchd service backend for macOS workstations.

use super::*;

/// Reads the unit's token credential into the environment, enters its working
/// directory, then execs the program: launchd has no credential store and no
/// `WorkingDirectory=` of its own that tracks a file rewritten at every token
/// rotation. Positional arguments: `$1` working dir (may be empty), `$2`
/// credential file (may be empty), then the argv. Values are taken verbatim
/// after the first `=`; nothing is evaluated.
pub(super) const LAUNCHD_ENTRY_SCRIPT: &str = "wd=\"$1\"; envf=\"$2\"; shift 2; \
if [ -n \"$wd\" ]; then cd \"$wd\" || exit 78; fi; \
if [ -n \"$envf\" ] && [ -r \"$envf\" ]; then \
while IFS= read -r l || [ -n \"$l\" ]; do \
case \"$l\" in *=*) export \"${l%%=*}=${l#*=}\";; esac; \
done < \"$envf\"; fi; \
exec \"$@\"";

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
/// `/bin/sh` running [`LAUNCHD_ENTRY_SCRIPT`], which reads the token
/// credential and enters the working directory before exec'ing the unit's argv.
pub fn render_launchd(label: &str, spec: &UnitSpec) -> String {
    let mut args: Vec<String> = vec![
        "-c".to_string(),
        LAUNCHD_ENTRY_SCRIPT.to_string(),
        "ados-plugin".to_string(),
        spec.working_dir
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        spec.credential
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
