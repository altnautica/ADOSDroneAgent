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

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

use ados_supervisor::process_manager::{render_plist, unit_to_label, PlistLogPaths};

use crate::cli::Args;
use crate::exec;
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
        ] {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("create {} failed", dir.display()))?;
        }
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
    build_and_install_binaries(&source, &paths)?;

    // 3. Identity + operator config + optional pairing material.
    let device_id = write_identity_and_config(args, &paths, &profile)?;

    // 4. Render + bootstrap the LaunchAgents.
    let uid = current_uid();
    let env = build_env(&paths, &device_id);
    register_launch_agents(&paths, &env, uid)?;

    // 5. Wait for the control surface to answer.
    let up = health_poll(CONTROL_PORT);

    print_summary(&paths, &device_id, uid, up);
    Ok(ExitCode::SUCCESS)
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
fn build_env(paths: &Paths, device_id: &str) -> Vec<(String, String)> {
    let run = paths.run.to_string_lossy().to_string();
    let config = paths.config.to_string_lossy().to_string();
    let node_id = format!("mac-{}", hostname_slug());
    let public_url = format!("http://{}:{COMPUTE_PORT}", lan_ipv4());
    vec![
        ("HOME".into(), paths.home.to_string_lossy().to_string()),
        ("PATH".into(), launchd_path()),
        ("RUST_LOG".into(), "info".into()),
        (
            "ADOS_HOME".into(),
            paths.ados_home.to_string_lossy().to_string(),
        ),
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

/// Poll the control surface until it answers on `port`, up to a bounded window.
/// Returns true once a completed HTTP response comes back (any status code proves
/// the daemon is listening); false if it never came up inside the window.
fn health_poll(port: u16) -> bool {
    let url = format!("http://127.0.0.1:{port}/api/status");
    print!("  health: waiting for ados-control on :{port} ");
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
        // curl exits 0 for a completed HTTP request regardless of the status
        // code (no `-f`), and prints a 3-digit code; a connection refused exits
        // non-zero. Either a 2xx or an auth/404 proves the port is up.
        if res.success() {
            if let Ok(code) = res.stdout.trim().parse::<u16>() {
                if code > 0 {
                    println!("✓ (HTTP {code})");
                    return true;
                }
            }
        }
        print!(".");
        let _ = std::io::stdout().flush();
        std::thread::sleep(Duration::from_millis(750));
    }
    println!(" (not yet answering)");
    false
}

/// Print the closing summary: how to reach the node, and how to tear it down.
fn print_summary(paths: &Paths, device_id: &str, uid: u32, up: bool) {
    let ip = lan_ipv4();
    println!();
    if up {
        println!("ADOS Workstation is UP.");
    } else {
        println!(
            "ADOS Workstation installed; the control surface has not answered yet \
             (launchd keeps retrying — check the logs below)."
        );
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
    println!("  Manage:");
    println!(
        "    launchctl print gui/{uid}/co.ados.control      # inspect the control surface job"
    );
    println!("    tail -f {}/ados-control.err.log", paths.log.display());
    println!();
    println!("  Tear down:");
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
        let env = build_env(&paths, "0011aabbccdd");
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
