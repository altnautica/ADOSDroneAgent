//! macOS workstation install path: a rootless, per-user launchd node.
//!
//! An Apple-Silicon Mac runs the ADOS agent's `workstation` profile as a set of
//! per-user LaunchAgents under `$HOME/.ados` — no root, no systemd, no apt, no
//! Python venv. The workstation profile is Rust-only: the native HTTP control
//! surface (`ados-control`, the LAN pairing + REST front on `:8080`), the shared
//! core daemons the supervisor orchestrates (`ados-supervisor`, `ados-cloud`,
//! `ados-logd`) and the plugin host (`ados-plugin-host`), which runs each
//! installed extension as its own LaunchAgent.
//!
//! There is no prebuilt Mach-O binary for these services, so the installer builds
//! them from the source tree (`cargo build --release`) into `$HOME/.ados/bin`,
//! writes the operator config + identity, renders one launchd plist per daemon
//! (via the same `ados_protocol::launchd` renderer and label mapper the runtime
//! `LaunchdManager` uses), bootstraps them into the user's GUI domain, and waits
//! for the control surface to answer.
//!
//! Every path hangs off `$HOME`; nothing writes under `/opt`, `/etc`, or
//! `/Library` (the root-owned FHS + system-domain launchd), so the whole flow
//! runs without a single `sudo` and with zero follow-up commands (the agents
//! `RunAtLoad` and `KeepAlive` on crash, so a reboot brings the node back).

use std::path::PathBuf;
use std::process::ExitCode;

use ados_protocol::launchd::current_uid;
use ados_term::exec;
use anyhow::{anyhow, Result};

use crate::cli::Args;

mod build;
mod health;
mod identity;
mod launchd;
mod paths;
mod report;

use build::{
    agent_version_from_source, build_and_install_binaries, ensure_cargo, install_brew_deps,
    install_cli, is_root, resolve_source_dir,
};
use health::{daemon_pid, health_poll};
use identity::{build_env, write_identity_and_config};
use launchd::{
    install_extensions, label_for, plugin_agent_labels, register_launch_agents,
    resolve_plugin_runner,
};
use report::{print_summary, write_macos_install_result};

/// The control surface's LAN port. The plist pins `ADOS_CONTROL_PORT=8080` so the
/// GCS reaches the workstation on the same port a drone/GS agent uses; the health
/// gate polls it.
const CONTROL_PORT: u16 = 8080;

/// One managed daemon: its service/binary name and whether launchd should keep it
/// alive across a crash. Every daemon is `RunAtLoad` (launchd starts it at load
/// and at login); `KeepAlive { Crashed: true }` restarts a crashed one.
struct Daemon {
    /// The binary + service name (also the `ados-<tail>` → `co.ados.<tail>` seed).
    name: &'static str,
    /// Restart on crash (every long-running daemon here wants this).
    keep_alive: bool,
}

/// The workstation daemon set registered as LaunchAgents. `ados-supervisor` is
/// the orchestrator; `ados-control` is the LAN front (`:8080`); `ados-cloud` is
/// the (idle-in-local-mode) relay; `ados-logd` is the durable logging store;
/// `ados-plugin-host` serves the per-plugin sockets and tokens. `ados-tui` is
/// built + installed but NOT registered — it is an interactive terminal UI, not
/// a background daemon.
const DAEMONS: &[Daemon] = &[
    Daemon {
        name: "ados-supervisor",
        keep_alive: true,
    },
    Daemon {
        name: "ados-control",
        keep_alive: true,
    },
    Daemon {
        name: "ados-cloud",
        keep_alive: true,
    },
    Daemon {
        name: "ados-logd",
        keep_alive: true,
    },
    Daemon {
        name: "ados-plugin-host",
        keep_alive: true,
    },
];

/// Every binary the workstation install builds from source and places under the
/// per-user bin dir. Mirrors `binaries::for_profile("workstation")` (what the
/// prebuilt catalog would fetch on an aarch64 SBC): the daemons above plus the
/// interactive `ados-tui`.
const WORKSTATION_BINARIES: &[&str] = &[
    "ados-supervisor",
    "ados-control",
    "ados-cloud",
    "ados-logd",
    "ados-plugin-host",
    "ados-tui",
];

/// Resolved per-user layout under `$HOME/.ados`.
struct Paths {
    home: PathBuf,
    ados_home: PathBuf,
    bin: PathBuf,
    run: PathBuf,
    log: PathBuf,
    config: PathBuf,
    profile_conf: PathBuf,
    device_id_file: PathBuf,
    pairing: PathBuf,
    launch_agents: PathBuf,
}

/// Drive the whole macOS workstation install. Returns the process exit code
/// (`SUCCESS` when the node is up, non-zero on a hard failure).
pub fn run(args: &Args) -> Result<ExitCode> {
    // A per-user launchd install must run as the target user, never root — a
    // root run would install the agents into root's `$HOME` and GUI domain.
    if is_root() {
        eprintln!(
            "error: on macOS run WITHOUT sudo — the ADOS workstation installs as \
             per-user LaunchAgents under your own $HOME/.ados"
        );
        return Ok(ExitCode::from(2));
    }

    let profile = resolve_profile(args)?;
    let paths = Paths::resolve()?;
    paths.ensure_dirs()?;

    println!("ADOS Workstation install (macOS, profile: {profile})");
    println!("  home: {}", paths.ados_home.display());

    // 1. Toolchain check + system deps (brew). Non-fatal: the node comes up
    //    without them; they only enable the video + accurate-reconstruction paths.
    ensure_cargo()?;
    install_brew_deps();

    // 2. Build the workstation binaries from source and place them under the
    //    per-user bin dir. Hard-fail: without the daemons there is no node.
    let source = resolve_source_dir()?;
    let agent_version = agent_version_from_source(&source);
    build_and_install_binaries(&source, &paths)?;

    // 2b. Install the `ados` management CLI so `ados update` / `status` / `logs`
    //     work on the workstation (the Rust node ships no `ados` command of its
    //     own). Best-effort: the daemons + the GCS work without it.
    install_cli(&source);

    // 3. Identity + operator config + optional pairing material.
    let device_id = write_identity_and_config(args, &paths, &profile)?;

    // 4. Render + bootstrap the LaunchAgents.
    let uid = current_uid();
    let runner = resolve_plugin_runner(&paths);
    let env = build_env(&paths, &device_id, &agent_version, &runner);
    register_launch_agents(&paths, &env, uid)?;

    // 5. Prove every daemon came up (not just that the control port answers).
    let report = health_poll(uid);

    // 5b. The World Engine extension, on a node that came up.
    install_extensions(args, &paths, &profile, &agent_version, &runner, &report);

    // 6. Record the outcome so the control surface can serve the real version and
    //    `ados status` / the CLI can read the recorded result.
    write_macos_install_result(&paths, &profile, &agent_version, &report);

    print_summary(&paths, &device_id, uid, &report);
    // Mirror the Linux path: a failed health gate exits non-zero so the operator
    // (and any curl-bash caller checking `$?`) sees the install did not come up.
    if report.up {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(1))
    }
}

/// Tear down a macOS workstation install: bootout every LaunchAgent (each
/// installed plugin's first, then the core daemons), remove the plists, and
/// (when `purge`) delete `$HOME/.ados` (identity + config + binaries).
pub fn uninstall(purge: bool) -> Result<ExitCode> {
    if is_root() {
        eprintln!("error: on macOS run the uninstall WITHOUT sudo (per-user agents)");
        return Ok(ExitCode::from(2));
    }
    let paths = Paths::resolve()?;
    let uid = current_uid();
    let labels = plugin_agent_labels(&paths.launch_agents)
        .into_iter()
        .chain(DAEMONS.iter().map(|d| label_for(d.name)));
    for label in labels {
        let target = format!("gui/{uid}/{label}");
        let _ = exec::run("launchctl", &["bootout", &target]);
        let plist = paths.launch_agents.join(format!("{label}.plist"));
        let _ = std::fs::remove_file(&plist);
    }
    println!("uninstall: booted out the ADOS LaunchAgents and removed their plists");
    if purge {
        let _ = std::fs::remove_dir_all(&paths.ados_home);
        println!("uninstall: purged {}", paths.ados_home.display());
    } else {
        println!(
            "uninstall: kept {} (identity + config); re-run with --force to purge it",
            paths.ados_home.display()
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// Report the install state of a macOS workstation node. Reads the per-user
/// `$HOME/.ados` layout (not the Linux FHS the shared `print_status` reads): the
/// recorded install-result contract, the operator config + resolved profile, and
/// the per-binary presence + live launchd state for the daemon set.
pub fn status(args: &Args) -> Result<ExitCode> {
    let paths = Paths::resolve()?;
    if !paths.ados_home.is_dir() {
        println!(
            "ADOS Workstation: not installed (no {})",
            paths.ados_home.display()
        );
        return Ok(ExitCode::SUCCESS);
    }
    println!("ADOS Workstation (macOS) — {}", paths.ados_home.display());

    // 1. The install-result contract (honour ADOS_INSTALL_RESULT so a test /
    //    operator override redirects it, else the per-user default).
    let result_path = std::env::var_os("ADOS_INSTALL_RESULT")
        .map(PathBuf::from)
        .unwrap_or_else(|| paths.install_result());
    match std::fs::read_to_string(&result_path) {
        Ok(body) => {
            println!("\ninstall-result ({}):", result_path.display());
            print!("{body}");
            if !body.ends_with('\n') {
                println!();
            }
        }
        Err(_) => {
            println!(
                "\ninstall-result: none at {} (no install recorded yet)",
                result_path.display()
            );
        }
    }

    // 2. Config presence + resolved profile.
    let profile = status_profile(args, &paths);
    let cfg = if paths.config.is_file() {
        "present"
    } else {
        "absent"
    };
    println!("\nconfig : {} ({cfg})", paths.config.display());
    println!("profile: {profile}");

    // 3. Per-binary presence + launchd running-state for the daemon set. `ados-tui`
    //    is installed but not a LaunchAgent, so it shows presence only.
    let uid = current_uid();
    println!("\nworkstation binaries ({}):", paths.bin.display());
    for name in WORKSTATION_BINARIES {
        let present = paths.bin.join(name).is_file();
        let mark = if present { "[x]" } else { "[ ]" };
        if DAEMONS.iter().any(|d| d.name == *name) {
            let run = match daemon_pid(uid, &label_for(name)) {
                Some(pid) => format!("running (pid {pid})"),
                None => "stopped".to_string(),
            };
            println!("  {mark} {name:<16} {run}");
        } else {
            println!("  {mark} {name:<16} (interactive, not a daemon)");
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Resolve the profile for `--status`: the `--profile` flag, else the persisted
/// `profile.conf` (`profile: <name>`), else `workstation`.
fn status_profile(args: &Args, paths: &Paths) -> String {
    if let Some(p) = args.profile.as_deref() {
        if !p.is_empty() {
            return p.to_string();
        }
    }
    if let Ok(body) = std::fs::read_to_string(&paths.profile_conf) {
        if let Some(p) = crate::env::parse_profile_conf(&body) {
            return p;
        }
    }
    "workstation".to_string()
}

/// Resolve the install profile. macOS runs only the operator's `workstation`
/// console or the lean `compute` worker — a drone/ground-station on a Mac is
/// nonsensical (no FC, no radio, no camera pipeline). Defaults to `workstation`.
fn resolve_profile(args: &Args) -> Result<String> {
    match args.profile.as_deref() {
        None => Ok("workstation".to_string()),
        Some("workstation") => Ok("workstation".to_string()),
        Some("compute") => Ok("compute".to_string()),
        Some(other) => Err(anyhow!(
            "profile '{other}' is not supported on macOS; use --profile workstation \
             (or compute). The drone / ground-station profiles need an SBC."
        )),
    }
}

/// The outcome of the health gate: whether the node is up, plus the human-readable
/// reasons any daemon failed (used both for the summary and the recorded
/// install-result's failed-step list).
struct HealthReport {
    up: bool,
    failures: Vec<String>,
}

/// The outcome of waiting for one launchd daemon to reach steady state.
enum SteadyOutcome {
    /// Held the same running pid across `STEADY_SAMPLES` consecutive samples.
    Stable,
    /// Was seen running at least once but never held a stable pid before the
    /// deadline — a crash-loop.
    CrashLooping,
    /// Never seen running under launchd before the deadline.
    NeverRunning,
}

#[cfg(test)]
mod tests;
