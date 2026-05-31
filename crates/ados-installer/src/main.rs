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

use ados_installer::checkpoint::Checkpoint;
use ados_installer::cli::{Args, RunMode, USAGE};
use ados_installer::ctx::Ctx;
use ados_installer::env::{EnvInfo, RESULT_PATH};
use ados_installer::graph::run_graph;
use ados_installer::journal::init_logging;
use ados_installer::result::{now_iso8601_utc, FailureAccumulator, InstallResult};
use ados_installer::steps::full_install_chain;

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
            // Stub: a later phase reads + prints the install-result contract.
            print_status();
            Ok(ExitCode::SUCCESS)
        }
        RunMode::Uninstall => {
            // Stub: a later phase performs the removal.
            tracing::info!("uninstall is not yet implemented in this build");
            println!("uninstall: not yet implemented");
            Ok(ExitCode::SUCCESS)
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
    let env = EnvInfo::probe();
    let checkpoint = Checkpoint::new();

    // A force reinstall clears the resume markers up front.
    if mode.clears_checkpoints() {
        if let Err(e) = checkpoint.clear() {
            tracing::warn!(error = %e, "failed to clear checkpoints before force reinstall");
        }
    }

    let mut ctx = Ctx::from_args(args, env, checkpoint);
    let profile = ctx.profile.clone();

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
    // /var/lib is not writable must not panic the binary).
    write_result(&ctx.failures, status, &profile);

    if status == "failed" {
        Ok(ExitCode::from(1))
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

/// Assemble + write the install-result contract. The board / kernel /
/// wfb-module-source fields are filled by later-phase probes; this scaffold
/// reports `unknown` / empty so the contract still lands and the abort path
/// always names the failed step(s).
fn write_result(failures: &FailureAccumulator, status: &str, profile: &str) {
    let result = InstallResult {
        status: status.to_string(),
        version: "unknown".to_string(),
        profile: profile.to_string(),
        board: "unknown".to_string(),
        kernel_release: kernel_release(),
        wfb_module_source: String::new(),
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

/// `uname -r`-equivalent. A later phase shells this out; the scaffold returns
/// `unknown` so the contract field is always present.
fn kernel_release() -> String {
    "unknown".to_string()
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

/// Stub status print (a later phase reads the install-result contract).
fn print_status() {
    println!("status: not yet implemented (reads {RESULT_PATH})");
}
