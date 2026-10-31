//! LaunchAgent registration, teardown helpers and extension installs.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ados_protocol::launchd::{render_plist, unit_to_label, PlistLogPaths};
use ados_term::exec;
use anyhow::{Context, Result};

use crate::cli::Args;
use crate::steps::extensions::{
    install_world_engine, world_engine_default, ExtensionOutcome, NodeTarget, DOWNLOAD_RETRY_DELAY,
    MANUAL_INSTALL,
};

use super::{HealthReport, Paths, DAEMONS};

/// Render, write, bootstrap, and kick every daemon's LaunchAgent. Idempotent: a
/// re-run boots the old job out first so `bootstrap` cannot fail on
/// "already loaded", then `kickstart -k` forces a fresh spawn on the new binary.
pub(super) fn register_launch_agents(
    paths: &Paths,
    env: &[(String, String)],
    uid: u32,
) -> Result<()> {
    for d in DAEMONS {
        let label = label_for(d.name);
        let program = paths.bin.join(d.name);
        let program_s = program.to_string_lossy().to_string();
        let stdout_log = paths.log.join(format!("{}.out.log", d.name));
        let stderr_log = paths.log.join(format!("{}.err.log", d.name));
        let plist_body = render_plist(
            &label,
            &program_s,
            &[],
            env,
            d.keep_alive,
            PlistLogPaths {
                stdout: Some(&stdout_log.to_string_lossy()),
                stderr: Some(&stderr_log.to_string_lossy()),
            },
        );
        let plist_path = paths.launch_agents.join(format!("{label}.plist"));
        std::fs::write(&plist_path, plist_body)
            .with_context(|| format!("writing {} failed", plist_path.display()))?;

        let target = format!("gui/{uid}/{label}");
        let domain = format!("gui/{uid}");
        // Idempotent teardown of a prior instance. `launchctl bootout` is
        // asynchronous — it returns before launchd has finished unloading the
        // job — so a `bootstrap` fired immediately after can race the teardown,
        // fail, and leave the service unloaded (kickstart then has nothing to
        // spawn). Only boot out a genuinely-loaded prior instance, and wait for
        // the label to disappear before re-bootstrapping.
        if exec::run("launchctl", &["print", &target]).success() {
            let _ = exec::run("launchctl", &["bootout", &target]);
            wait_label_gone(&target);
        }
        // Load + (RunAtLoad) start.
        let plist_s = plist_path.to_string_lossy().to_string();
        let boot = exec::run("launchctl", &["bootstrap", &domain, &plist_s]);
        if !boot.success() {
            // Not fatal: an "already bootstrapped" or a transient error still lets
            // kickstart bring it up, and the health gate is the real proof.
            tracing::warn!(label, code = ?boot.code, stderr = %boot.stderr.trim(), "launchctl bootstrap non-zero");
        }
        // Ensure the label is enabled (a prior `disable` would otherwise block it).
        let _ = exec::run("launchctl", &["enable", &target]);
        // Force a fresh spawn on the freshly-installed binary + env.
        let _ = exec::run("launchctl", &["kickstart", "-k", &target]);
        println!("  launchd: {label} → {}", program.display());
    }
    Ok(())
}

/// Poll until a launchd label is no longer loaded (bounded ~5s), so a
/// re-bootstrap does not race launchd's asynchronous `bootout`. `launchctl
/// print <target>` exits zero while the job is loaded, non-zero once it is gone.
fn wait_label_gone(target: &str) {
    for _ in 0..50 {
        if !exec::run("launchctl", &["print", target]).success() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// `co.ados.<tail>` label for an `ados-<tail>` service name (the shared mapper,
/// so the installer and the supervisor agree on the label).
pub(super) fn label_for(name: &str) -> String {
    unit_to_label(name)
}

/// The label prefix every plugin LaunchAgent carries (`co.ados.plugin.<token>`,
/// as the plugin host's launchd backend writes them). The dot keeps it apart
/// from the core `co.ados.plugin-host` daemon.
const PLUGIN_AGENT_PREFIX: &str = "co.ados.plugin.";

/// The labels of every plugin LaunchAgent under `launch_agents`, read from the
/// plist file names, sorted. An unreadable dir has none.
pub(super) fn plugin_agent_labels(launch_agents: &Path) -> Vec<String> {
    let mut labels: Vec<String> = std::fs::read_dir(launch_agents)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.strip_suffix(".plist")
                .filter(|label| label.starts_with(PLUGIN_AGENT_PREFIX))
                .map(str::to_string)
        })
        .collect();
    labels.sort();
    labels
}

/// The `co.ados.<tail>` tails of the core daemons, space-separated, for the
/// summary's shell teardown line. Derived from [`DAEMONS`] so it names exactly
/// the jobs this install registered.
pub(super) fn teardown_daemon_names() -> String {
    DAEMONS
        .iter()
        .map(|d| d.name.strip_prefix("ados-").unwrap_or(d.name))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The Python plugin runner a Python-runtime plugin's LaunchAgent execs: the
/// `ados-plugin-runner` console script the `ados` package installs beside the
/// `ados` command, resolved on this shell's PATH. When it is not on PATH the
/// per-user bin path is used and a note says Python-runtime plugins will not
/// start until the runner is installed; Rust-runtime plugins never exec it.
pub(super) fn resolve_plugin_runner(paths: &Paths) -> PathBuf {
    let res = exec::run("sh", &["-c", "command -v ados-plugin-runner"]);
    let found = res.stdout.trim();
    if res.success() && !found.is_empty() {
        return PathBuf::from(found);
    }
    let fallback = paths.bin.join("ados-plugin-runner");
    println!(
        "  plugins: ados-plugin-runner is not on PATH; Python-runtime plugins will not start \
         until the `ados` package's runner is installed (expected at {})",
        fallback.display()
    );
    fallback
}

/// Install the World Engine extension when the operator (`--world-engine`) or
/// the profile default asked for it: the same controller, bundled catalog and
/// sha256 pin as the Linux `extensions` step. Runs only on a node that came up,
/// since the extension's agent half talks to the plugin host. Best-effort: an
/// unpublished extension or a failed install prints the manual command and
/// never fails the install.
pub(super) fn install_extensions(
    args: &Args,
    paths: &Paths,
    profile: &str,
    agent_version: &str,
    runner: &Path,
    report: &HealthReport,
) {
    if !args
        .world_engine
        .unwrap_or_else(|| world_engine_default(profile))
    {
        println!("  extensions: World Engine not selected");
        return;
    }
    if !report.up {
        println!(
            "  extensions: World Engine skipped because the node did not come up; install it \
             later with `{MANUAL_INSTALL}`"
        );
        return;
    }
    println!("  extensions: installing the World Engine …");
    let result = install_world_engine(NodeTarget {
        paths: paths.plugin_paths(runner),
        profile: ados_config::normalize_profile(Some(profile)),
        agent_version: agent_version.to_string(),
        // The HAL sidecar under the daemons' ADOS_RUN_DIR.
        board_sidecar: paths.run.join("board.json"),
        // launchd enforces no sandbox, so a network grant never waits on the
        // Linux-only loopback guard.
        guard_wait: Duration::ZERO,
        retry_delay: DOWNLOAD_RETRY_DELAY,
    });
    match result {
        Ok(ExtensionOutcome::Installed { version, .. }) => {
            println!("  extensions: World Engine {version} installed and enabled")
        }
        Ok(ExtensionOutcome::AlreadyInstalled { version }) => println!(
            "  extensions: World Engine {version} already installed; plugin auto-update keeps \
             it current"
        ),
        Ok(ExtensionOutcome::Finished { version, .. }) => println!(
            "  extensions: World Engine {version} was installed but off; required permissions \
             granted and enabled"
        ),
        Ok(ExtensionOutcome::Unavailable(reason)) | Err(reason) => println!(
            "  extensions: World Engine not installed: {reason}. Install it later with \
             `{MANUAL_INSTALL}`"
        ),
    }
}
