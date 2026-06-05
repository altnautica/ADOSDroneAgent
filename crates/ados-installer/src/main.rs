//! Entry point. Parses argv, resolves the run mode, assembles the step chain,
//! drives it through the graph engine, then writes the install-result contract
//! and exits with a code derived from the outcome.
//!
//! Exit codes: 0 for `ok` / `degraded` (the agent is up, possibly with a
//! best-effort capability missing), non-zero for `failed` (a Required step
//! failed and the install aborted).

use std::path::Path;
use std::process::ExitCode;

use anyhow::Result;

use ados_installer::binaries;
use ados_installer::checkpoint::Checkpoint;
use ados_installer::cli::{Args, RunMode, USAGE};
use ados_installer::ctx::Ctx;
use ados_installer::env::{self, EnvInfo, RESULT_PATH};
use ados_installer::exec;
use ados_installer::graph::run_graph;
use ados_installer::journal::init_logging;
use ados_installer::result::{now_iso8601_utc, FailureAccumulator, InstallResult};
use ados_installer::steps::full_install_chain;
use ados_installer::ui;
use ados_installer::uninstall;

#[tokio::main]
async fn main() -> Result<ExitCode> {
    init_logging();

    let args = match Args::from_env() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            return Ok(ExitCode::from(2));
        }
    };

    if args.help {
        print!("{USAGE}");
        return Ok(ExitCode::SUCCESS);
    }

    // "already installed" probing is a later phase; pass false so the
    // flag-driven path resolves (a bare pair code on a fresh box installs).
    let mode = RunMode::resolve(&args, false);
    tracing::info!(?mode, "resolved install run-mode");

    match mode {
        RunMode::Status => {
            print_status(&args);
            Ok(ExitCode::SUCCESS)
        }
        RunMode::Uninstall => {
            // `--force` doubles as the purge flag here (remove /etc/ados too)
            // so a `--uninstall --force` wipes identity for a from-clean reinstall.
            let purge = args.force;
            match uninstall::run_uninstall(purge) {
                Ok(()) => {
                    println!(
                        "uninstall: done{}",
                        if purge { " (purged /etc/ados)" } else { "" }
                    );
                    Ok(ExitCode::SUCCESS)
                }
                Err(e) => {
                    eprintln!("uninstall: {e}");
                    Ok(ExitCode::from(1))
                }
            }
        }
        RunMode::PairOnly => {
            // Stub: a later phase re-pairs against the running agent.
            tracing::info!("pair-only is not yet implemented in this build");
            println!("pair-only: not yet implemented");
            Ok(ExitCode::SUCCESS)
        }
        RunMode::FreshInstall | RunMode::Upgrade | RunMode::ForceReinstall => {
            run_install(args, mode)
        }
    }
}

/// Drive the install step chain and write the result contract.
fn run_install(args: Args, mode: RunMode) -> Result<ExitCode> {
    // Start the live progress UI before any work so the operator sees feedback
    // immediately. The mode is chosen from stderr + the environment.
    let render_mode = ui::detect_mode(&args);
    let theme = ui::Theme::detect(args.no_color, args.ascii);
    let profile_hint = args.profile.clone().unwrap_or_else(|| "drone".to_string());
    let header = format!("Installing the ADOS Drone Agent ({profile_hint})…");
    let (sink, render) = ui::start(render_mode, header, theme);

    let env = EnvInfo::probe();
    let checkpoint = Checkpoint::new();

    // A force reinstall clears the resume markers up front.
    if mode.clears_checkpoints() {
        if let Err(e) = checkpoint.clear() {
            tracing::warn!(error = %e, "failed to clear checkpoints before force reinstall");
        }
    }

    let mut ctx = Ctx::from_args(args, env, checkpoint);
    ctx.progress = sink.clone();

    let reports = run_graph(full_install_chain(), &mut ctx);
    let status = ctx.failures.derive_status();
    tracing::info!(
        status,
        steps = reports.len(),
        failed = ctx.failures.failed.len(),
        required = ctx.failures.required.len(),
        "install finished"
    );

    // Build + write the result contract (best-effort: a dev host where
    // /var/lib is not writable must not panic the binary). The profile was
    // resolved into ctx by preflight, so read it back from there.
    write_result(&ctx.failures, status, &ctx.profile);

    // Hand the renderer the closing summary, then wait for it to draw the
    // success card / failure panel and restore the terminal.
    if render_mode == ui::RenderMode::Json {
        print_result_json();
    }
    sink.summary(build_summary(status, &ctx));
    sink.finish();
    render.finish();

    if status == "failed" {
        Ok(ExitCode::from(1))
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

/// Assemble the renderer's closing-summary payload from the resolved context.
fn build_summary(status: &str, ctx: &Ctx) -> ui::SummaryData {
    let hostname = read_hostname();
    let setup_url = format!("http://{hostname}.local:8080/setup");
    ui::SummaryData {
        status: status.to_string(),
        version: installed_version(),
        profile: ctx.profile.clone(),
        board: board_id(),
        device_id: read_device_id(),
        hostname,
        setup_url,
        paired: pairing_present(),
        failed_steps: ctx.failures.failed.clone(),
        required_failures: ctx.failures.required.clone(),
    }
}

/// Hostname for the `<host>.local` hint + setup URL: `/etc/hostname`, then
/// `uname -n`, else `ados`.
fn read_hostname() -> String {
    if let Ok(s) = std::fs::read_to_string("/etc/hostname") {
        let v = s.trim();
        if !v.is_empty() {
            return v.to_string();
        }
    }
    let res = exec::run("uname", &["-n"]);
    if res.success() {
        let v = res.stdout.trim();
        if !v.is_empty() {
            return v.to_string();
        }
    }
    "ados".to_string()
}

/// The 12-hex device id, or `unknown`.
fn read_device_id() -> String {
    std::fs::read_to_string(env::DEVICE_ID_FILE)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Whether pairing material is present on disk.
fn pairing_present() -> bool {
    std::fs::metadata(env::PAIRING_JSON)
        .map(|m| m.len() > 0)
        .unwrap_or(false)
}

/// Print the install-result contract to stdout (for `--json`).
fn print_result_json() {
    if let Ok(body) = std::fs::read_to_string(RESULT_PATH) {
        print!("{body}");
    }
}

/// Assemble + write the install-result contract with the real probed fields.
fn write_result(failures: &FailureAccumulator, status: &str, profile: &str) {
    let result = InstallResult {
        status: status.to_string(),
        version: installed_version(),
        profile: profile.to_string(),
        board: board_id(),
        kernel_release: kernel_release(),
        wfb_module_source: wfb_module_source(),
        failed_steps: failures.failed.clone(),
        required_failures: failures.required.clone(),
        ts: now_iso8601_utc(),
    };

    let path = Path::new(RESULT_PATH);
    if !result_path_writable(path) {
        tracing::warn!(path = %RESULT_PATH, "result path not writable; skipping result write (dev host?)");
        return;
    }
    if let Err(e) = result.write_atomic(path) {
        tracing::warn!(error = %e, path = %RESULT_PATH, "failed to write install result");
    }
}

/// The installed agent version, read straight from the package's `__version__`
/// (mirrors the bash `get_installed_version`). `unknown` when the venv is
/// absent or cannot import the package.
fn installed_version() -> String {
    let py = format!("{}/bin/python", env::VENV_DIR);
    let res = exec::run(&py, &["-c", "import ados; print(ados.__version__)"]);
    if res.success() {
        let v = res.stdout.trim();
        if !v.is_empty() {
            return v.to_string();
        }
    }
    "unknown".to_string()
}

/// The board id: the persisted override sentinel first, then the device-tree
/// model, else `unknown` (mirrors the bash `write_install_result` board read).
fn board_id() -> String {
    if let Ok(s) = std::fs::read_to_string("/etc/ados/board_override") {
        let v = s.trim().trim_matches('\0').trim();
        if !v.is_empty() {
            return v.to_string();
        }
    }
    if let Ok(s) = std::fs::read_to_string("/proc/device-tree/model") {
        // The device-tree model is NUL-terminated; strip NULs + trim.
        let v = s.replace('\0', "");
        let v = v.trim();
        if !v.is_empty() {
            return v.to_string();
        }
    }
    "unknown".to_string()
}

/// `uname -r`. Shells out via the exec primitive; `unknown` on any failure.
fn kernel_release() -> String {
    let res = exec::run("uname", &["-r"]);
    if res.success() {
        let v = res.stdout.trim();
        if !v.is_empty() {
            return v.to_string();
        }
    }
    "unknown".to_string()
}

/// The WFB module source sentinel the driver step / driver script wrote
/// (`prebuilt` | `dkms`), or empty when no RTL adapter / driver is present.
fn wfb_module_source() -> String {
    std::fs::read_to_string("/run/ados/wfb-module-source")
        .map(|s| s.trim().trim_matches('\0').trim().to_string())
        .unwrap_or_default()
}

/// True when the result file's parent directory exists and is writable. Guards
/// the write so a dev host (no `/var/lib/ados`) does not error.
fn result_path_writable(path: &Path) -> bool {
    match path.parent() {
        Some(parent) => parent.is_dir() && is_writable(parent),
        None => false,
    }
}

#[cfg(target_os = "linux")]
fn is_writable(dir: &Path) -> bool {
    use nix::unistd::{access, AccessFlags};
    access(dir, AccessFlags::W_OK).is_ok()
}

#[cfg(not(target_os = "linux"))]
fn is_writable(dir: &Path) -> bool {
    // Off Linux: probe by metadata read-only flag. Conservative — a false
    // negative only skips a result write on a dev host.
    std::fs::metadata(dir)
        .map(|m| !m.permissions().readonly())
        .unwrap_or(false)
}

/// Print install status: the install-result contract (if present), the
/// completed checkpoints, and the per-binary presence for the resolved profile.
fn print_status(args: &Args) {
    // 1. The install-result contract.
    match std::fs::read_to_string(RESULT_PATH) {
        Ok(body) => {
            println!("install-result ({RESULT_PATH}):");
            print!("{body}");
            if !body.ends_with('\n') {
                println!();
            }
        }
        Err(_) => {
            println!("install-result: none at {RESULT_PATH} (no install recorded yet)");
        }
    }

    // 2. Completed checkpoints.
    let checkpoint = Checkpoint::new();
    let done = checkpoint.list();
    if done.is_empty() {
        println!("\ncheckpoints: none");
    } else {
        println!("\ncheckpoints completed:");
        for cp in &done {
            println!("  [x] {cp}");
        }
    }

    // 3. Per-binary presence for the profile. The profile flag wins; else the
    // persisted profile.conf; else drone (matches preflight's resolution).
    let profile = resolve_status_profile(args);
    println!("\nprebuilt binaries ({profile}):");
    for b in binaries::for_profile(&profile) {
        let present = std::fs::metadata(b.dest)
            .map(|m| m.is_file())
            .unwrap_or(false);
        let mark = if present { "[x]" } else { "[ ]" };
        let gate = match b.gate {
            binaries::Gate::Hard => "hard",
            binaries::Gate::BestEffort => "best-effort",
        };
        println!("  {mark} {} ({gate})", b.service);
    }
}

/// Resolve the profile for `--status` reporting: `--profile` flag, else the
/// persisted `profile.conf`, else `drone`. Reuses preflight's pure resolver.
fn resolve_status_profile(args: &Args) -> String {
    let conf_body = std::fs::read_to_string(env::PROFILE_CONF).ok();
    ados_installer::steps::preflight::resolve_profile(args.profile.as_deref(), conf_body.as_deref())
}
