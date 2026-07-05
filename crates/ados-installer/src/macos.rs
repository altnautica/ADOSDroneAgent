//! macOS workstation install path: a rootless, per-user launchd node.
//!
//! An Apple-Silicon Mac runs the ADOS agent's `workstation` profile as a set of
//! per-user LaunchAgents under `$HOME/.ados` — no root, no systemd, no apt, no
//! Python venv. The workstation profile is Rust-only: the native HTTP control
//! surface (`ados-control`, the LAN pairing + REST front on `:8080`) and the
//! compute engine (`ados-compute`, the reconstruct / offload job API on `:8092`),
//! plus the shared core daemons the supervisor orchestrates (`ados-supervisor`,
//! `ados-cloud`, `ados-logd`).
//!
//! There is no prebuilt Mach-O binary for these services, so the installer builds
//! them from the source tree (`cargo build --release`) into `$HOME/.ados/bin`,
//! writes the operator config + identity, renders one launchd plist per daemon
//! (via the same `render_plist` the runtime `LaunchdManager` manages), bootstraps
//! them into the user's GUI domain, and waits for the control surface to answer.
//!
//! Every path hangs off `$HOME`; nothing writes under `/opt`, `/etc`, or
//! `/Library` (the root-owned FHS + system-domain launchd), so the whole flow
//! runs without a single `sudo` and with zero follow-up commands (the agents
//! `RunAtLoad` and `KeepAlive` on crash, so a reboot brings the node back).

use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};

use ados_supervisor::process_manager::{render_plist, unit_to_label, PlistLogPaths};

use crate::cli::Args;
use crate::exec;
use crate::result::{now_iso8601_utc, InstallResult};
use crate::steps::config_identity::pairing_json;

/// The control surface's LAN port. The plist pins `ADOS_CONTROL_PORT=8080` so the
/// GCS reaches the workstation on the same port a drone/GS agent uses; the health
/// gate polls it.
const CONTROL_PORT: u16 = 8080;
/// The compute engine's job-API bind port (mirrors the dev-node contract).
const COMPUTE_PORT: u16 = 8092;

/// One managed daemon: its service/binary name and whether launchd should keep it
/// alive across a crash. All five are `RunAtLoad` (launchd starts them at load
/// and at login); `KeepAlive { Crashed: true }` restarts a crashed one.
struct Daemon {
    /// The binary + service name (also the `ados-<tail>` → `co.ados.<tail>` seed).
    name: &'static str,
    /// Restart on crash (every long-running daemon here wants this).
    keep_alive: bool,
}

/// The workstation daemon set registered as LaunchAgents. `ados-supervisor` is
/// the orchestrator; `ados-control` is the LAN front (`:8080`); `ados-compute`
/// is the engine (`:8092`); `ados-cloud` is the (idle-in-local-mode) relay;
/// `ados-logd` is the durable logging store. `ados-tui` is built + installed but
/// NOT registered — it is an interactive terminal UI, not a background daemon.
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
        name: "ados-compute",
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
];

/// Every binary the workstation install builds from source and places under the
/// per-user bin dir. Mirrors `binaries::for_profile("workstation")` (the six the
/// prebuilt catalog would fetch on an aarch64 SBC): the five daemons above plus
/// the interactive `ados-tui`.
const WORKSTATION_BINARIES: &[&str] = &[
    "ados-supervisor",
    "ados-control",
    "ados-compute",
    "ados-cloud",
    "ados-logd",
    "ados-tui",
];

/// Resolved per-user layout under `$HOME/.ados`.
struct Paths {
    home: PathBuf,
    ados_home: PathBuf,
    bin: PathBuf,
    run: PathBuf,
    compute: PathBuf,
    log: PathBuf,
    config: PathBuf,
    profile_conf: PathBuf,
    device_id_file: PathBuf,
    pairing: PathBuf,
    launch_agents: PathBuf,
}

impl Paths {
    /// Resolve the layout from `$HOME`. Errors when `$HOME` is unset (a rootless
    /// per-user install has nowhere to land otherwise).
    fn resolve() -> Result<Self> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or_else(|| {
                anyhow!("$HOME is not set; the per-user install needs a home directory")
            })?;
        let ados_home = home.join(".ados");
        Ok(Paths {
            bin: ados_home.join("bin"),
            run: ados_home.join("run"),
            compute: ados_home.join("compute"),
            log: ados_home.join("log"),
            config: ados_home.join("config.yaml"),
            profile_conf: ados_home.join("profile.conf"),
            device_id_file: ados_home.join("device-id"),
            pairing: ados_home.join("pairing.json"),
            launch_agents: home.join("Library/LaunchAgents"),
            ados_home,
            home,
        })
    }

    /// The config/identity dir the native control surface reads via `ADOS_ETC_DIR`
    /// (Linux `/etc/ados`). Rooted at `$HOME/.ados` so the Rust installer's
    /// top-level `config.yaml` / `pairing.json` / `device-id` / `profile.conf`
    /// resolve to the same files the Python CLI reads, with the ground-station
    /// side-files + secrets landing beside them (writable, never root-owned).
    fn etc_dir(&self) -> PathBuf {
        self.ados_home.clone()
    }

    /// The vision models dir (`ADOS_MODELS_DIR`, Linux `/opt/ados/models/vision`).
    fn models_dir(&self) -> PathBuf {
        self.ados_home.join("models")
    }

    /// The recordings dir (`ADOS_RECORDINGS_DIR`, Linux `/var/ados/recordings`).
    fn recordings_dir(&self) -> PathBuf {
        self.ados_home.join("recordings")
    }

    /// The WFB key dir (`ADOS_WFB_KEY_DIR`, Linux `/etc/ados/wfb`).
    fn wfb_key_dir(&self) -> PathBuf {
        self.ados_home.join("wfb-keys")
    }

    /// The owner-only secrets dir (holds the setup token under `ADOS_SETUP_TOKEN_PATH`).
    fn secrets_dir(&self) -> PathBuf {
        self.ados_home.join("secrets")
    }

    /// The setup token path (`ADOS_SETUP_TOKEN_PATH`, Linux `/etc/ados/secrets/setup-token`).
    fn setup_token(&self) -> PathBuf {
        self.secrets_dir().join("setup-token")
    }

    /// The logging store's parent dir (the DB lands at `logd/logs.db`).
    fn logd_dir(&self) -> PathBuf {
        self.ados_home.join("logd")
    }

    /// The install-result contract path (`ADOS_INSTALL_RESULT`, Linux
    /// `/var/lib/ados/install-result.json`). The control surface reads its version
    /// from here; `ados status` and the CLI read it for the recorded outcome.
    fn install_result(&self) -> PathBuf {
        self.ados_home.join("install-result.json")
    }

    /// Create the directories the install writes into.
    fn ensure_dirs(&self) -> Result<()> {
        for dir in [
            &self.ados_home,
            &self.bin,
            &self.run,
            &self.compute,
            &self.compute.join("work"),
            &self.log,
            &self.launch_agents,
            // The logging store's DB dir, created explicitly so a create failure
            // surfaces here rather than being swallowed by the daemon's own
            // best-effort `create_dir_all`.
            &self.logd_dir(),
            // The remaining home-rooted dirs the native control surface reads via
            // their ADOS_* overrides, so no route falls back to a root-owned Linux
            // path (`/opt/ados/models`, `/var/ados/recordings`, `/etc/ados/wfb`).
            &self.models_dir(),
            &self.recordings_dir(),
            &self.wfb_key_dir(),
            &self.secrets_dir(),
        ] {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("create {} failed", dir.display()))?;
        }
        // The secrets dir holds owner-only material; lock it down.
        set_mode(&self.secrets_dir(), 0o700);
        Ok(())
    }
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
    let env = build_env(&paths, &device_id, &agent_version);
    register_launch_agents(&paths, &env, uid)?;

    // 5. Prove every daemon came up (not just that the control port answers).
    let report = health_poll(uid);

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

/// Tear down a macOS workstation install: bootout every LaunchAgent, remove the
/// plists, and (when `purge`) delete `$HOME/.ados` (identity + config + binaries).
pub fn uninstall(purge: bool) -> Result<ExitCode> {
    if is_root() {
        eprintln!("error: on macOS run the uninstall WITHOUT sudo (per-user agents)");
        return Ok(ExitCode::from(2));
    }
    let paths = Paths::resolve()?;
    let uid = current_uid();
    for d in DAEMONS {
        let label = label_for(d.name);
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
        for line in body.lines() {
            if let Some(rest) = line.trim().strip_prefix("profile:") {
                let v = rest.trim().trim_matches('"');
                if !v.is_empty() {
                    return v.to_string();
                }
            }
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

/// True when running as root (euid 0). Read via `id -u` so no platform-gated libc
/// dependency is pulled into the pure-logic crate.
fn is_root() -> bool {
    exec::run("id", &["-u"]).stdout.trim() == "0"
}

/// The effective uid for the `gui/<uid>` launchd domain. Falls back to 501 (the
/// first macOS user account) only if `id -u` is somehow unreadable.
fn current_uid() -> u32 {
    exec::run("id", &["-u"])
        .stdout
        .trim()
        .parse::<u32>()
        .unwrap_or(501)
}

/// Ensure `cargo` is reachable (the install builds the service binaries from
/// source). Hard-fail with an actionable hint when it is missing.
fn ensure_cargo() -> Result<()> {
    if exec::run_ok("sh", &["-c", "command -v cargo"]) {
        return Ok(());
    }
    Err(anyhow!(
        "cargo not found on PATH. Install the Rust toolchain (https://rustup.rs) \
         and re-run — the macOS workstation builds its binaries from source."
    ))
}

/// Install the system dependencies via Homebrew, best-effort. `ffmpeg` backs the
/// video + capture paths; `colmap` seeds the accurate gaussian-splat
/// reconstruction (the engine falls back to a working random-init path without
/// it). Neither is required for the node to come up, so a missing `brew` or a
/// slow bottle only degrades — it never blocks the install.
fn install_brew_deps() {
    if !exec::run_ok("sh", &["-c", "command -v brew"]) {
        println!(
            "  deps: Homebrew not found; skipping ffmpeg/colmap \
             (install brew + `brew install ffmpeg colmap` to enable video + accurate reconstruction)"
        );
        return;
    }
    // ffmpeg is a small bottle; install it so the video pipeline works.
    if have_tool("ffmpeg") {
        println!("  deps: ffmpeg already present");
    } else {
        println!("  deps: brew install ffmpeg …");
        if !exec::run_ok("brew", &["install", "ffmpeg"]) {
            println!("  deps: ffmpeg install reported a non-zero status (continuing)");
        }
    }
    // colmap is the accurate-reconstruction seed. It is heavy to build from
    // source, so only pull it when a bottle keeps it quick; otherwise leave a
    // one-line note (the engine's random-init path still works).
    if have_tool("colmap") {
        println!("  deps: colmap already present (accurate reconstruction seed)");
    } else {
        println!(
            "  deps: colmap not installed; the reconstruction engine uses its \
             random-init path (run `brew install colmap` for the accurate seed)"
        );
    }
}

/// Install the `ados` management CLI (a Python console script) from the source
/// tree so `ados update` / `ados status` / `ados logs` / `ados uninstall` work on
/// the workstation — the Rust node ships no `ados` command of its own. Runs on
/// every upgrade so the CLI tracks the agent. Best-effort: the daemons and the
/// GCS work without it, so a missing python3/pip or a pip failure warns loudly
/// with the manual command rather than failing the whole install.
fn install_cli(source: &Path) {
    if !exec::run_ok("sh", &["-c", "command -v python3"]) {
        println!(
            "  cli: python3 not found — the `ados` command was not installed. \
             Install Python 3, then run: python3 -m pip install {}",
            source.display()
        );
        return;
    }
    if !exec::run_ok("python3", &["-m", "pip", "--version"]) {
        println!(
            "  cli: pip is unavailable under python3 — the `ados` command was not \
             installed. Install pip, then run: python3 -m pip install {}",
            source.display()
        );
        return;
    }
    println!("  cli: installing the ados command (pip) …");
    let src = source.to_string_lossy().to_string();
    let res = exec::run("python3", &["-m", "pip", "install", "--upgrade", &src]);
    if res.success() {
        println!("  cli: ados command ready");
    } else {
        let why = res
            .stderr
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim();
        println!("  cli: could not install the `ados` command automatically ({why}).");
        println!(
            "       Run it by hand: python3 -m pip install {}",
            source.display()
        );
    }
}

/// True when a tool resolves on PATH.
fn have_tool(tool: &str) -> bool {
    exec::run_ok("sh", &["-c", &format!("command -v {tool}")])
}

/// Resolve the source tree to build from: the `ADOS_SOURCE_DIR` the bootstrap
/// script exports, else a walk up from the current directory for the workspace
/// manifest (`crates/ados-installer/Cargo.toml`). Errors when neither resolves.
fn resolve_source_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("ADOS_SOURCE_DIR") {
        let p = PathBuf::from(dir);
        if p.join("crates/ados-installer/Cargo.toml").is_file() {
            return Ok(p);
        }
    }
    let mut cur = std::env::current_dir().context("reading the current directory")?;
    loop {
        if cur.join("crates/ados-installer/Cargo.toml").is_file() {
            return Ok(cur);
        }
        // Also accept being invoked from inside the `crates` dir.
        if cur.join("ados-installer/Cargo.toml").is_file() {
            if let Some(parent) = cur.parent() {
                return Ok(parent.to_path_buf());
            }
        }
        match cur.parent() {
            Some(p) => cur = p.to_path_buf(),
            None => break,
        }
    }
    Err(anyhow!(
        "could not locate the ADOS source tree; set ADOS_SOURCE_DIR to the repo root \
         (the dir containing crates/ados-installer/Cargo.toml)"
    ))
}

/// Build the workstation binaries from source (`cargo build --release`) and place
/// each under the per-user bin dir. Hard-fail: a broken build or a missing output
/// binary means there is no node to run.
fn build_and_install_binaries(source: &Path, paths: &Paths) -> Result<()> {
    let manifest = source.join("crates/Cargo.toml");
    let manifest_s = manifest.to_string_lossy().to_string();

    println!(
        "  build: cargo build --release ({} crates) …",
        WORKSTATION_BINARIES.len()
    );
    let mut argv: Vec<String> = vec![
        "build".to_string(),
        "--release".to_string(),
        "--manifest-path".to_string(),
        manifest_s,
    ];
    for name in WORKSTATION_BINARIES {
        argv.push("-p".to_string());
        argv.push((*name).to_string());
    }
    let argv_ref: Vec<&str> = argv.iter().map(String::as_str).collect();
    // Stream the build to the operator's terminal (a source build is slow; silent
    // would look hung). `run` captures output, so use a status-only spawn here.
    let status = std::process::Command::new("cargo")
        .args(&argv_ref)
        .status()
        .context("spawning cargo build")?;
    if !status.success() {
        return Err(anyhow!(
            "cargo build --release failed ({}); fix the build and re-run",
            status
        ));
    }

    // Resolve the release output dir (respecting CARGO_TARGET_DIR like the dev
    // node contract), then copy each binary into the per-user bin dir.
    let target_root = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| source.join("crates/target"));
    let release_dir = target_root.join("release");

    for name in WORKSTATION_BINARIES {
        let built = release_dir.join(name);
        if !built.is_file() {
            return Err(anyhow!(
                "expected built binary not found: {} (the build did not produce {name})",
                built.display()
            ));
        }
        let dest = paths.bin.join(name);
        // Atomic-ish replace: copy to a temp sibling then rename over the dest so a
        // running daemon keeps its old inode through an upgrade.
        let tmp = paths.bin.join(format!(".{name}.new"));
        std::fs::copy(&built, &tmp)
            .with_context(|| format!("copy {} -> {} failed", built.display(), tmp.display()))?;
        set_executable(&tmp);
        std::fs::rename(&tmp, &dest)
            .with_context(|| format!("install {} failed", dest.display()))?;
    }
    println!(
        "  build: installed {} binaries to {}",
        WORKSTATION_BINARIES.len(),
        paths.bin.display()
    );
    Ok(())
}

/// Mint/persist the device id, write the workstation `config.yaml` (skip-if-exists,
/// so an upgrade preserves operator edits), the `profile.conf`, and — when
/// `--pair` was given — the pairing material. Returns the resolved device id.
fn write_identity_and_config(args: &Args, paths: &Paths, profile: &str) -> Result<String> {
    // Device id: never overwrite an existing one (the node's identity).
    let device_id = match std::fs::read_to_string(&paths.device_id_file) {
        Ok(existing) if !existing.trim().is_empty() => existing.trim().to_string(),
        _ => {
            let id = mint_device_id();
            std::fs::write(&paths.device_id_file, format!("{id}\n"))
                .with_context(|| format!("writing {} failed", paths.device_id_file.display()))?;
            id
        }
    };
    let short_id: String = device_id.chars().take(8).collect();

    // profile.conf so a re-run resolves the same profile.
    let _ = std::fs::write(&paths.profile_conf, format!("profile: {profile}\n"));

    // config.yaml: skip-if-exists (preserve operator edits / upgrade).
    if paths.config.exists() {
        println!(
            "  config: {} exists; leaving it intact",
            paths.config.display()
        );
    } else {
        let name = args.name.clone().unwrap_or_else(hostname_full);
        std::fs::write(
            &paths.config,
            workstation_config_yaml(&short_id, &name, profile),
        )
        .with_context(|| format!("writing {} failed", paths.config.display()))?;
        set_mode(&paths.config, 0o600);
        println!("  config: wrote {}", paths.config.display());
    }

    // Pairing material (only when --pair given).
    if let Some(code) = args.pair.as_deref() {
        let body = pairing_json(code, now_epoch());
        std::fs::write(&paths.pairing, body)
            .with_context(|| format!("writing {} failed", paths.pairing.display()))?;
        set_mode(&paths.pairing, 0o600);
        println!("  pairing: wrote code {}", code.to_ascii_uppercase());
    }

    Ok(device_id)
}

/// Build the workstation `config.yaml` body (pure). A Rust-only workstation node:
/// identity + local server posture + atlas enabled (the reconstruction surface).
fn workstation_config_yaml(short_id: &str, name: &str, profile: &str) -> String {
    format!(
        "# ADOS Workstation Configuration\n\
# Generated by ados-installer (macOS, per-user)\n\
\n\
agent:\n  \
device_id: \"{short_id}\"\n  \
name: \"{name}\"\n  \
profile: \"{profile}\"\n  \
tier: \"auto\"\n\
\n\
server:\n  \
mode: \"local\"\n  \
heartbeat_interval: 5\n\
\n\
# The world-model reconstruction surface. Enabled so the compute engine serves\n\
# the Atlas job API; the drone/GCS submits reconstruction + offload work to it.\n\
atlas:\n  \
enabled: true\n"
    )
}

/// The shared environment every daemon plist carries. Mirrors the dev-node
/// contract (`scripts/dev/run-compute-node-macos.sh`) so the LAN-paired GCS
/// reaches `ados-control` on `:8080` and the compute card reads the heartbeat
/// sidecar under the per-user run dir — every path pinned under `$HOME/.ados`.
fn build_env(paths: &Paths, device_id: &str, agent_version: &str) -> Vec<(String, String)> {
    let run = paths.run.to_string_lossy().to_string();
    let config = paths.config.to_string_lossy().to_string();
    let node_id = format!("mac-{}", hostname_slug());
    let public_url = format!("http://{}:{COMPUTE_PORT}", lan_ipv4());
    let path = |p: PathBuf| p.to_string_lossy().to_string();
    vec![
        ("HOME".into(), paths.home.to_string_lossy().to_string()),
        ("PATH".into(), launchd_path()),
        ("RUST_LOG".into(), "info".into()),
        (
            "ADOS_HOME".into(),
            paths.ados_home.to_string_lossy().to_string(),
        ),
        // The agent version the control surface reports on `/api/status` (it reads
        // ADOS_AGENT_VERSION first). A Rust-only workstation has no pip package to
        // read `ados.__version__` from, so thread the source-tree version here.
        ("ADOS_AGENT_VERSION".into(), agent_version.to_string()),
        // The config/identity + data dirs the native control surface reads, rooted
        // under the writable per-user home so no route falls back to a root-owned
        // Linux path (`/etc/ados`, `/opt/ados/models`, `/var/ados/recordings`,
        // `/etc/ados/wfb`, `/var/lib/ados`, `/etc/ados/secrets`).
        ("ADOS_ETC_DIR".into(), path(paths.etc_dir())),
        ("ADOS_MODELS_DIR".into(), path(paths.models_dir())),
        ("ADOS_RECORDINGS_DIR".into(), path(paths.recordings_dir())),
        ("ADOS_WFB_KEY_DIR".into(), path(paths.wfb_key_dir())),
        ("ADOS_INSTALL_RESULT".into(), path(paths.install_result())),
        ("ADOS_SETUP_TOKEN_PATH".into(), path(paths.setup_token())),
        // The logging store's durable DB. Linux roots it at `/var/ados`, which a
        // rootless per-user install cannot create; point it at the writable home.
        (
            "ADOS_LOGD_DB".into(),
            paths
                .ados_home
                .join("logd/logs.db")
                .to_string_lossy()
                .to_string(),
        ),
        ("ADOS_RUN_DIR".into(), run.clone()),
        ("ADOS_CONFIG".into(), config.clone()),
        ("ADOS_CONFIG_YAML".into(), config),
        (
            "ADOS_PROFILE_CONF".into(),
            paths.profile_conf.to_string_lossy().to_string(),
        ),
        (
            "ADOS_PAIRING_JSON".into(),
            paths.pairing.to_string_lossy().to_string(),
        ),
        ("ADOS_DEVICE_ID".into(), device_id.to_string()),
        ("ADOS_CONTROL_SOCKET".into(), format!("{run}/control.sock")),
        ("ADOS_CONTROL_PORT".into(), CONTROL_PORT.to_string()),
        (
            "ADOS_COMPUTE_DB".into(),
            paths.compute.join("jobs.db").to_string_lossy().to_string(),
        ),
        (
            "ADOS_COMPUTE_WORK".into(),
            paths.compute.join("work").to_string_lossy().to_string(),
        ),
        (
            "ADOS_COMPUTE_BIND".into(),
            format!("0.0.0.0:{COMPUTE_PORT}"),
        ),
        ("ADOS_COMPUTE_NODE_ID".into(), node_id),
        ("ADOS_COMPUTE_PUBLIC_URL".into(), public_url),
        ("ADOS_ATLAS_ENABLED".into(), "1".into()),
    ]
}

/// A PATH for the launchd job environment that includes Homebrew (Apple-silicon
/// `/opt/homebrew`, Intel `/usr/local`) so a daemon can find `ffmpeg` / `colmap`
/// / `git`. launchd's default job PATH omits the Homebrew prefixes.
fn launchd_path() -> String {
    "/opt/homebrew/bin:/opt/homebrew/sbin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin".into()
}

/// Render, write, bootstrap, and kick every daemon's LaunchAgent. Idempotent: a
/// re-run boots the old job out first so `bootstrap` cannot fail on
/// "already loaded", then `kickstart -k` forces a fresh spawn on the new binary.
fn register_launch_agents(paths: &Paths, env: &[(String, String)], uid: u32) -> Result<()> {
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

/// `co.ados.<tail>` label for an `ados-<tail>` service name (reuses the runtime
/// backend's mapper so the installer and the supervisor agree on the label).
fn label_for(name: &str) -> String {
    unit_to_label(name)
}

/// The outcome of the health gate: whether the node is up, plus the human-readable
/// reasons any daemon failed (used both for the summary and the recorded
/// install-result's failed-step list).
struct HealthReport {
    up: bool,
    failures: Vec<String>,
}

/// Prove the whole workstation node came up, not merely that the control port
/// answers. Every one of the five daemons must be loaded + running under launchd
/// AND stay running across a re-sample (a `ados-logd` that crash-loops on its DB —
/// the original silent-failure class — is caught by the pid changing / vanishing),
/// `ados-control` must return a 2xx from `/api/status` (not just any HTTP code),
/// and `ados-compute` must actually be listening on its job-API port.
fn health_poll(uid: u32) -> HealthReport {
    // The control surface is the slowest to answer (it builds its full router), so
    // waiting for its 2xx also gives the other daemons time to reach steady state
    // before the launchd re-sample below.
    let control_ok = wait_control_2xx(CONTROL_PORT);
    let compute_ok = wait_port_open(COMPUTE_PORT);

    // Wait for each launchd daemon to reach STEADY STATE — the same running pid
    // held across several consecutive samples. A fresh KeepAlive job commonly
    // restarts once or twice while it initializes (it waits on a socket, a
    // dependency, or its DB before it settles), so a single before/after snapshot
    // is too fragile: it false-fails a healthy node that merely settled during the
    // 2s window (a Rule-44 lying surface — a false "down" is as bad as a false
    // "up"). Instead each daemon gets a settle window to stop restarting. Only a
    // daemon that never holds a stable pid within the window — a genuine
    // crash-loop, e.g. `ados-logd` on an unwritable DB path, which exits on
    // startup and never accumulates the streak — or one that is never running
    // fails the gate.
    let labels: Vec<(&str, String)> = DAEMONS
        .iter()
        .map(|d| (d.name, label_for(d.name)))
        .collect();

    let mut failures = Vec::new();
    for (name, label) in &labels {
        match wait_daemon_steady(uid, label) {
            SteadyOutcome::Stable => {
                // Combine the launchd steady-state proof with the functional proof
                // for the two daemons that have one.
                if *name == "ados-control" && !control_ok {
                    failures.push(format!(
                        "{name}: not returning 2xx on :{CONTROL_PORT}/api/status"
                    ));
                } else if *name == "ados-compute" && !compute_ok {
                    failures.push(format!("{name}: not listening on :{COMPUTE_PORT}"));
                }
            }
            SteadyOutcome::CrashLooping => failures.push(format!(
                "{name}: still restarting after {}s, crash-looping",
                STEADY_DEADLINE.as_secs()
            )),
            SteadyOutcome::NeverRunning => {
                failures.push(format!("{name}: not running under launchd"))
            }
        }
    }

    if failures.is_empty() {
        println!(
            "  health: all {} daemons up (control 2xx, compute :{COMPUTE_PORT}, launchd running+stable)",
            DAEMONS.len()
        );
    } else {
        println!("  health: FAILED");
        for f in &failures {
            println!("    - {f}");
        }
    }
    HealthReport {
        up: failures.is_empty(),
        failures,
    }
}

/// Consecutive identical running-pid samples that prove a daemon has settled.
/// At the sample cadence below this is ~2.4s of no restart — long enough that an
/// immediately-crash-looping daemon (which exits within ms and is throttled by
/// launchd to a ≥10s respawn interval) can never accumulate the streak, while a
/// daemon that restarted once during init reaches it quickly.
const STEADY_SAMPLES: u32 = 4;
/// The gap between pid samples while waiting for steady state.
const STEADY_SAMPLE_GAP: Duration = Duration::from_millis(600);
/// The per-daemon settle deadline. Comfortably longer than launchd's default
/// ≥10s respawn throttle so a crash-looping daemon shows at least one not-running
/// gap (which resets the streak) within the window; a healthy daemon settles in
/// ~2.4s so the common case returns fast.
const STEADY_DEADLINE: Duration = Duration::from_secs(20);

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

/// Poll a daemon's launchd pid until it holds steady (`STEADY_SAMPLES` consecutive
/// identical running pids) or the settle deadline elapses. A changed or vanished
/// pid resets the streak, so only a daemon that stops restarting passes.
fn wait_daemon_steady(uid: u32, label: &str) -> SteadyOutcome {
    let start = Instant::now();
    let mut last: Option<i64> = None;
    let mut streak: u32 = 0;
    let mut ever_running = false;
    loop {
        match daemon_pid(uid, label) {
            Some(pid) => {
                ever_running = true;
                if last == Some(pid) {
                    streak += 1;
                } else {
                    last = Some(pid);
                    streak = 1;
                }
                if streak >= STEADY_SAMPLES {
                    return SteadyOutcome::Stable;
                }
            }
            None => {
                // Not currently running — between KeepAlive respawns, or never
                // started. Either way the settle streak breaks.
                last = None;
                streak = 0;
            }
        }
        if start.elapsed() >= STEADY_DEADLINE {
            return if ever_running {
                SteadyOutcome::CrashLooping
            } else {
                SteadyOutcome::NeverRunning
            };
        }
        std::thread::sleep(STEADY_SAMPLE_GAP);
    }
}

/// Poll `GET /api/status` until it returns a 2xx (bounded ~30s). A 2xx — not just
/// any completed HTTP code — is the proof the control surface is actually serving,
/// so a `500`/`502` from a control daemon that came up but cannot answer does not
/// pass the gate. Connection-refused (daemon not listening) exits curl non-zero.
fn wait_control_2xx(port: u16) -> bool {
    let url = format!("http://127.0.0.1:{port}/api/status");
    print!("  health: waiting for ados-control (2xx) on :{port} ");
    use std::io::Write;
    let _ = std::io::stdout().flush();
    for _ in 0..40 {
        let res = exec::run(
            "curl",
            &[
                "-sS",
                "-o",
                "/dev/null",
                "-w",
                "%{http_code}",
                "--max-time",
                "3",
                &url,
            ],
        );
        if res.success() {
            if let Ok(code) = res.stdout.trim().parse::<u16>() {
                if (200..300).contains(&code) {
                    println!("ok (HTTP {code})");
                    return true;
                }
            }
        }
        print!(".");
        let _ = std::io::stdout().flush();
        std::thread::sleep(Duration::from_millis(750));
    }
    println!(" (no 2xx)");
    false
}

/// Poll a loopback TCP port until a connection is accepted (bounded ~20s). Proves
/// the daemon that should own the port (`ados-compute` on its job-API port) is
/// actually listening, not merely loaded under launchd.
fn wait_port_open(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    for _ in 0..20 {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    false
}

/// The running pid of a launchd job, or `None` when it is not loaded or not
/// currently running. `launchctl print gui/<uid>/<label>` exits zero while the job
/// is loaded and prints the job's own `state = running` + `pid = N` in its header
/// block while its process is alive; a crash-looping job shows a changing pid (or a
/// not-running state) across samples, which is how the health gate catches it.
///
/// The job's `state`/`pid` are the FIRST such lines in the header. The rest of the
/// output nests sub-dictionaries (endpoints, sockets, spawn env) that carry their
/// OWN `state = active`/`state = inactive` lines — reading every `state =` line
/// would let a trailing nested `state = active` clobber the job's `running` flag to
/// `false` and make a healthy daemon read as down (a Rule-44 lying surface). So take
/// only the first occurrence of each.
fn daemon_pid(uid: u32, label: &str) -> Option<i64> {
    let target = format!("gui/{uid}/{label}");
    let res = exec::run("launchctl", &["print", &target]);
    if !res.success() {
        return None;
    }
    parse_daemon_state(&res.stdout)
}

/// Parse the running pid out of `launchctl print` output: the first `state =`
/// (the job's own state) must be `running`, and the first `pid =` is the job pid.
/// Split out so the nested-`state`-clobber regression is unit-testable without a
/// live launchd.
fn parse_daemon_state(stdout: &str) -> Option<i64> {
    let mut running: Option<bool> = None;
    let mut pid: Option<i64> = None;
    for line in stdout.lines() {
        let t = line.trim();
        if running.is_none() {
            if let Some(v) = t.strip_prefix("state = ") {
                running = Some(v.trim() == "running");
                continue;
            }
        }
        if pid.is_none() {
            if let Some(v) = t.strip_prefix("pid = ") {
                pid = v.trim().parse::<i64>().ok();
            }
        }
    }
    if running == Some(true) {
        pid
    } else {
        None
    }
}

/// Parse the agent version out of the source tree's `src/ados/__init__.py`
/// (`__version__ = "x.y.z"`). This is the version of the codebase the workstation
/// binaries were built from — the honest value to record + report, since a
/// Rust-only Mac node has no installed pip package to read it from. `unknown` when
/// the file is absent or carries no parseable version.
fn agent_version_from_source(source: &Path) -> String {
    let init = source.join("src/ados/__init__.py");
    if let Ok(body) = std::fs::read_to_string(&init) {
        for line in body.lines() {
            let t = line.trim();
            if t.starts_with("__version__") {
                if let Some(rest) = t.split_once('=').map(|(_, r)| r) {
                    let v = rest.trim().trim_matches(['"', '\'']);
                    if !v.is_empty() {
                        return v.to_string();
                    }
                }
            }
        }
    }
    "unknown".to_string()
}

/// Write the install-result contract to `$HOME/.ados/install-result.json` so the
/// control surface serves the real version and `ados status` reports the outcome.
/// Health failures are recorded as required failures so the status is `failed`
/// when a daemon did not come up.
fn write_macos_install_result(paths: &Paths, profile: &str, version: &str, report: &HealthReport) {
    let status = if report.up { "ok" } else { "failed" };
    let result = InstallResult {
        status: status.to_string(),
        version: version.to_string(),
        profile: profile.to_string(),
        board: format!("macos-{}", std::env::consts::ARCH),
        kernel_release: kernel_release(),
        wfb_module_source: String::new(),
        failed_steps: report.failures.clone(),
        required_failures: report.failures.clone(),
        ts: now_iso8601_utc(),
    };
    let path = paths.install_result();
    if let Err(e) = result.write_atomic(&path) {
        tracing::warn!(error = %e, path = %path.display(), "failed to write macOS install result");
    } else {
        println!("  result: wrote {}", path.display());
    }
}

/// `uname -r` (the Darwin kernel release), or `unknown` on any failure.
fn kernel_release() -> String {
    let res = exec::run("uname", &["-r"]);
    let v = res.stdout.trim();
    if res.success() && !v.is_empty() {
        v.to_string()
    } else {
        "unknown".to_string()
    }
}

/// Print the closing summary: how to reach the node, and how to tear it down.
fn print_summary(paths: &Paths, device_id: &str, uid: u32, report: &HealthReport) {
    let ip = lan_ipv4();
    println!();
    if report.up {
        println!("ADOS Workstation is UP.");
    } else {
        println!(
            "ADOS Workstation install did NOT come up cleanly ({} daemon issue{}):",
            report.failures.len(),
            if report.failures.len() == 1 { "" } else { "s" }
        );
        for f in &report.failures {
            println!("  - {f}");
        }
        println!("launchd keeps retrying the KeepAlive daemons — check the logs below.");
    }
    println!("  device id     : {device_id}");
    println!("  control (GCS) : http://127.0.0.1:{CONTROL_PORT}   http://{ip}:{CONTROL_PORT}");
    println!("  compute       : http://127.0.0.1:{COMPUTE_PORT}");
    println!("  logs          : {}/", paths.log.display());
    println!(
        "  agents        : {}/co.ados.*.plist",
        paths.launch_agents.display()
    );
    println!();
    // The everyday `ados` commands — the same curated list the Linux summary and
    // `ados help` render, so all three surfaces agree.
    println!("  Useful commands:");
    for (cmd, desc) in crate::ui::summary::NEXT_STEPS {
        println!("    {cmd:<16} {desc}");
    }
    println!();
    println!("  Manage:");
    println!(
        "    launchctl print gui/{uid}/co.ados.control      # inspect the control surface job"
    );
    println!("    tail -f {}/ados-control.err.log", paths.log.display());
    println!();
    // Fallback teardown if the `ados` CLI did not install (best-effort on macOS).
    println!("  Tear down (or: ados uninstall --purge):");
    println!("    for s in supervisor control compute cloud logd; do launchctl bootout gui/{uid}/co.ados.$s 2>/dev/null; done");
    println!("    rm {}/co.ados.*.plist", paths.launch_agents.display());
    println!("    rm -rf {}", paths.ados_home.display());
}

// ── small helpers ────────────────────────────────────────────────────────────

/// Generate a fresh 12-hex device id from `getrandom` (no `/proc` on macOS).
fn mint_device_id() -> String {
    let mut bytes = [0u8; 6];
    if getrandom::getrandom(&mut bytes).is_ok() {
        return hex::encode(bytes);
    }
    let pid = std::process::id();
    format!("{pid:012x}").chars().take(12).collect()
}

/// The host's full name (`hostname`), or `ados-workstation` on failure.
fn hostname_full() -> String {
    let res = exec::run("hostname", &[]);
    let v = res.stdout.trim();
    if res.success() && !v.is_empty() {
        v.to_string()
    } else {
        "ados-workstation".to_string()
    }
}

/// A DNS-safe slug of the short hostname for the compute node id (`mac-<slug>`).
fn hostname_slug() -> String {
    let res = exec::run("hostname", &["-s"]);
    let raw = res.stdout.trim().to_ascii_lowercase();
    let slug: String = raw
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        "node".to_string()
    } else {
        slug
    }
}

/// This host's LAN IPv4 (`en0`, then `en1`), or `127.0.0.1`. Used for the
/// browser-reachable artifact base + the operator reach block.
fn lan_ipv4() -> String {
    for iface in ["en0", "en1"] {
        let res = exec::run("ipconfig", &["getifaddr", iface]);
        let v = res.stdout.trim();
        if res.success() && !v.is_empty() {
            return v.to_string();
        }
    }
    "127.0.0.1".to_string()
}

/// Seconds since the Unix epoch (for the pairing stamp).
fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// chmod a path (Unix).
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

/// chmod 0755 a freshly-installed binary.
fn set_executable(path: &Path) {
    set_mode(path, 0o755);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_daemon_state_ignores_nested_endpoint_state_lines() {
        // Real `launchctl print` shape: the job's own `state = running` + `pid`
        // come first, then nested sub-dictionaries carry their own `state =`
        // lines. Reading every `state =` line let a trailing nested
        // `state = active` clobber the job flag to false and read a healthy
        // daemon as down. The parser must take only the first of each.
        let out = "\
gui/501/co.ados.control = {
\tactive count = 1
\tstate = running
\tprogram = /Users/x/.ados/bin/ados-control
\tpid = 82787
\tendpoints = {
\t\t\"co.ados.control.socket\" = {
\t\t\tport = 8080
\t\t\tstate = active
\t\t}
\t}
\tsockets = {
\t\t\"listener\" = {
\t\t\tstate = active
\t\t}
\t}
}";
        assert_eq!(parse_daemon_state(out), Some(82787));

        // A genuinely stopped job (state not running) reads as None even when a
        // stale pid line is present.
        let stopped = "\
gui/501/co.ados.logd = {
\tstate = not running
\tpid = 999
\tendpoints = {
\t\tx = { state = active }
\t}
}";
        assert_eq!(parse_daemon_state(stopped), None);

        // No state line at all (job not loaded) reads as None.
        assert_eq!(parse_daemon_state("gui/501/co.ados.x = {\n}"), None);
    }

    #[test]
    fn profile_defaults_to_workstation_and_rejects_sbc_profiles() {
        let ws = Args::default();
        assert_eq!(resolve_profile(&ws).unwrap(), "workstation");

        let compute = Args {
            profile: Some("compute".to_string()),
            ..Args::default()
        };
        assert_eq!(resolve_profile(&compute).unwrap(), "compute");

        let explicit_ws = Args {
            profile: Some("workstation".to_string()),
            ..Args::default()
        };
        assert_eq!(resolve_profile(&explicit_ws).unwrap(), "workstation");

        // A drone/ground-station profile on a Mac is an error, not a silent default.
        let drone = Args {
            profile: Some("drone".to_string()),
            ..Args::default()
        };
        assert!(resolve_profile(&drone).is_err());
        let gs = Args {
            profile: Some("ground_station".to_string()),
            ..Args::default()
        };
        assert!(resolve_profile(&gs).is_err());
    }

    #[test]
    fn workstation_config_carries_identity_and_atlas() {
        let cfg = workstation_config_yaml("abcd1234", "my-mac", "workstation");
        assert!(cfg.contains("device_id: \"abcd1234\""));
        assert!(cfg.contains("name: \"my-mac\""));
        assert!(cfg.contains("profile: \"workstation\""));
        assert!(cfg.contains("mode: \"local\""));
        assert!(cfg.contains("enabled: true"));
    }

    #[test]
    fn label_maps_service_to_reverse_dns() {
        assert_eq!(label_for("ados-control"), "co.ados.control");
        assert_eq!(label_for("ados-compute"), "co.ados.compute");
        assert_eq!(label_for("ados-supervisor"), "co.ados.supervisor");
        // The reverse-DNS prefix is what the plist filename + service target use.
        assert!(label_for("ados-cloud").starts_with("co.ados."));
    }

    #[test]
    fn env_pins_the_control_port_and_home_rooted_paths() {
        let home = PathBuf::from("/Users/tester");
        let paths = Paths {
            bin: home.join(".ados/bin"),
            run: home.join(".ados/run"),
            compute: home.join(".ados/compute"),
            log: home.join(".ados/log"),
            config: home.join(".ados/config.yaml"),
            profile_conf: home.join(".ados/profile.conf"),
            device_id_file: home.join(".ados/device-id"),
            pairing: home.join(".ados/pairing.json"),
            launch_agents: home.join("Library/LaunchAgents"),
            ados_home: home.join(".ados"),
            home,
        };
        let env = build_env(&paths, "0011aabbccdd", "1.2.3");
        let get = |k: &str| env.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
        assert_eq!(get("ADOS_CONTROL_PORT").as_deref(), Some("8080"));
        assert_eq!(
            get("ADOS_CONFIG").as_deref(),
            Some("/Users/tester/.ados/config.yaml")
        );
        assert_eq!(
            get("ADOS_RUN_DIR").as_deref(),
            Some("/Users/tester/.ados/run")
        );
        assert_eq!(get("ADOS_DEVICE_ID").as_deref(), Some("0011aabbccdd"));
        assert_eq!(get("ADOS_ATLAS_ENABLED").as_deref(), Some("1"));
        assert_eq!(
            get("ADOS_LOGD_DB").as_deref(),
            Some("/Users/tester/.ados/logd/logs.db")
        );
        // The compute config path uses the compute-specific env var name.
        assert_eq!(
            get("ADOS_CONFIG_YAML").as_deref(),
            Some("/Users/tester/.ados/config.yaml")
        );
        // The version the control surface reports for a Rust-only Mac node.
        assert_eq!(get("ADOS_AGENT_VERSION").as_deref(), Some("1.2.3"));
        // The etc/data dirs the control surface reads, all home-rooted so no route
        // falls back to a root-owned Linux path.
        assert_eq!(get("ADOS_ETC_DIR").as_deref(), Some("/Users/tester/.ados"));
        assert_eq!(
            get("ADOS_MODELS_DIR").as_deref(),
            Some("/Users/tester/.ados/models")
        );
        assert_eq!(
            get("ADOS_RECORDINGS_DIR").as_deref(),
            Some("/Users/tester/.ados/recordings")
        );
        assert_eq!(
            get("ADOS_WFB_KEY_DIR").as_deref(),
            Some("/Users/tester/.ados/wfb-keys")
        );
        assert_eq!(
            get("ADOS_INSTALL_RESULT").as_deref(),
            Some("/Users/tester/.ados/install-result.json")
        );
        assert_eq!(
            get("ADOS_SETUP_TOKEN_PATH").as_deref(),
            Some("/Users/tester/.ados/secrets/setup-token")
        );
        // The launchd PATH must carry the Homebrew prefixes so a daemon finds
        // ffmpeg/colmap/git.
        assert!(get("PATH").unwrap().contains("/opt/homebrew/bin"));
    }

    #[test]
    fn mint_device_id_is_12_hex() {
        let id = mint_device_id();
        assert_eq!(id.len(), 12);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn agent_version_parses_dunder_version() {
        let dir = tempfile::tempdir().unwrap();
        let init = dir.path().join("src/ados/__init__.py");
        std::fs::create_dir_all(init.parent().unwrap()).unwrap();
        std::fs::write(
            &init,
            "\"\"\"doc\"\"\"\n\n__version__ = \"0.99.86\"\n\nfoo = 1\n",
        )
        .unwrap();
        assert_eq!(agent_version_from_source(dir.path()), "0.99.86");

        // A tree with no package version reads as unknown, never a panic.
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(agent_version_from_source(empty.path()), "unknown");
    }

    #[test]
    fn install_result_records_health_and_lands_at_home() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let paths = Paths {
            bin: home.join(".ados/bin"),
            run: home.join(".ados/run"),
            compute: home.join(".ados/compute"),
            log: home.join(".ados/log"),
            config: home.join(".ados/config.yaml"),
            profile_conf: home.join(".ados/profile.conf"),
            device_id_file: home.join(".ados/device-id"),
            pairing: home.join(".ados/pairing.json"),
            launch_agents: home.join("Library/LaunchAgents"),
            ados_home: home.join(".ados"),
            home,
        };
        std::fs::create_dir_all(&paths.ados_home).unwrap();

        // A clean install records status ok with no failed steps.
        let ok = HealthReport {
            up: true,
            failures: vec![],
        };
        write_macos_install_result(&paths, "workstation", "1.2.3", &ok);
        let body = std::fs::read_to_string(paths.install_result()).unwrap();
        assert!(body.contains("\"status\": \"ok\""));
        assert!(body.contains("\"version\": \"1.2.3\""));
        assert!(body.contains("\"profile\": \"workstation\""));
        assert!(body.contains("\"failedSteps\": []"));

        // A failed health gate records status failed and carries the reasons as
        // both failed + required steps (so the recorded status is `failed`).
        let bad = HealthReport {
            up: false,
            failures: vec!["ados-logd: restarting".to_string()],
        };
        write_macos_install_result(&paths, "workstation", "1.2.3", &bad);
        let body = std::fs::read_to_string(paths.install_result()).unwrap();
        assert!(body.contains("\"status\": \"failed\""));
        assert!(body.contains("ados-logd: restarting"));
    }

    #[test]
    fn status_profile_prefers_flag_then_conf_then_default() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let paths = Paths {
            bin: home.join(".ados/bin"),
            run: home.join(".ados/run"),
            compute: home.join(".ados/compute"),
            log: home.join(".ados/log"),
            config: home.join(".ados/config.yaml"),
            profile_conf: home.join(".ados/profile.conf"),
            device_id_file: home.join(".ados/device-id"),
            pairing: home.join(".ados/pairing.json"),
            launch_agents: home.join("Library/LaunchAgents"),
            ados_home: home.join(".ados"),
            home,
        };
        std::fs::create_dir_all(&paths.ados_home).unwrap();

        // No flag, no conf → the default.
        let none = Args::default();
        assert_eq!(status_profile(&none, &paths), "workstation");

        // profile.conf wins when no flag.
        std::fs::write(&paths.profile_conf, "profile: compute\n").unwrap();
        assert_eq!(status_profile(&none, &paths), "compute");

        // An explicit flag wins over the conf.
        let flagged = Args {
            profile: Some("workstation".to_string()),
            ..Args::default()
        };
        assert_eq!(status_profile(&flagged, &paths), "workstation");
    }

    #[test]
    fn hostname_slug_is_dns_safe() {
        // The slug helper shells to `hostname`, so just assert the shape it must
        // always produce (non-empty, lowercase alnum/dash, no leading/trailing dash).
        let s = hostname_slug();
        assert!(!s.is_empty());
        assert!(!s.starts_with('-') && !s.ends_with('-'));
        assert!(s
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
    }
}
