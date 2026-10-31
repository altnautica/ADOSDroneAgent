//! Entry point. Parses argv, resolves the run mode, assembles the step chain,
//! drives it through the graph engine, then writes the install-result contract
//! and exits with a code derived from the outcome.
//!
//! Exit codes: 0 for `ok` / `degraded` (the agent is up, possibly with a
//! best-effort capability missing), non-zero for `failed` (a Required step
//! failed and the install aborted).

use std::process::ExitCode;

use anyhow::Result;

use ados_installer::checkpoint::Checkpoint;
use ados_installer::cli::{render_flags, usage, Args, RunMode};
use ados_installer::ctx::{artifacts_conflict, resolve_channel, rev_channel_conflict, Ctx};
use ados_installer::env::{self, EnvInfo};
use ados_installer::graph::run_graph;
use ados_installer::journal::init_logging;
use ados_installer::steps::full_install_chain;
use ados_installer::ui::{groups::INSTALL_SPEC, groups::UNINSTALL_SPEC};
use ados_installer::wizard::{self, WizardControl};
use ados_installer::{pair_only, report, status, uninstall};
use ados_term::{exec, progress, progress::RenderMode, theme::Theme, tty::Tty};

/// Ignore `SIGHUP` so a controlling-terminal hangup (a dropped SSH session)
/// cannot kill the running install. Best-effort and idempotent; a failure to
/// install the disposition just leaves the default. Linux-only: the macOS path
/// runs under launchd, not a foreground SSH pty.
#[cfg(target_os = "linux")]
fn ignore_sighup() {
    use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};
    let action = SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty());
    // Safety: SigIgn installs no handler function, so there is nothing
    // async-signal-unsafe to run.
    unsafe {
        let _ = sigaction(Signal::SIGHUP, &action);
    }
}

#[cfg(not(target_os = "linux"))]
fn ignore_sighup() {}

#[tokio::main]
async fn main() -> Result<ExitCode> {
    init_logging();
    // Survive a dropped controlling terminal. An install run over SSH sits in the
    // foreground process group of the operator's pty; if a mid-install network
    // reconfig churns that link (e.g. a Wi-Fi IP change), the pty hangup delivers
    // SIGHUP to the install and kills it — stranding a half-completed state.
    // Ignoring SIGHUP lets every install step finish regardless of the operator's
    // link; the interactive UI keeps its own /dev/tty handle and degrades cleanly
    // if that terminal goes away.
    ignore_sighup();

    let args = match Args::from_env() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}\n\n{}", usage());
            return Ok(ExitCode::from(2));
        }
    };

    if args.help {
        print!("{}", usage());
        return Ok(ExitCode::SUCCESS);
    }

    // Probe whether the agent is already installed on this box (venv + deployed
    // supervisor unit + persisted identity). A bare `--pair CODE` against an
    // existing install resolves to a fast re-pair; on a fresh box the same
    // invocation runs the full install (which pairs at the end).
    let already_installed = env::probe_install_present();
    let mode = RunMode::resolve(&args, already_installed);
    tracing::info!(?mode, already_installed, "resolved install run-mode");

    // A revision pin has nothing to apply on macOS: that path builds every binary
    // from the source tree it is invoked in (`macos::build_and_install_binaries`)
    // and fetches no prebuilt asset and no wheel, so the pin IS the tree, and
    // `scripts/install.sh` is what resolves and checks it out before exec'ing this
    // binary — which is also why it strips the flag. Accepting it here would
    // silently ignore a pin the operator was told they had; it used to be rejected
    // as an unknown flag, and it must stay just as loud now that the Linux path
    // knows the flag.
    #[cfg(target_os = "macos")]
    if let Some(rev) = args.rev.as_deref() {
        eprintln!(
            "error: --ref is resolved by scripts/install.sh on macOS, not by the installer. \
             Run `scripts/install.sh --ref {rev}` to pin the tree it builds from, or check \
             {rev} out yourself and re-run without --ref."
        );
        return Ok(ExitCode::from(2));
    }

    // Same reasoning for `--artifacts`: the macOS path builds every workstation
    // binary from the source tree it is invoked in, so there is no fetch for a
    // local artifact directory to displace. Accepting it would report a source
    // selection that had no effect.
    #[cfg(target_os = "macos")]
    if let Some(dir) = args.artifacts.as_deref() {
        eprintln!(
            "error: --artifacts has nothing to apply on macOS: this path builds \
             every binary from the source tree it runs in and fetches no \
             prebuilt asset. Build the tree you want ({dir} is not consulted) \
             and re-run without --artifacts."
        );
        return Ok(ExitCode::from(2));
    }

    // macOS is a rootless per-user launchd node: the install builds the
    // workstation binaries from source and registers LaunchAgents under
    // `$HOME/.ados`, and the uninstall boots them out — neither touches the Linux
    // systemd / apt / venv / FHS paths the shared handlers below assume. Dispatch
    // those modes (plus Status, which must read `$HOME/.ados` not the Linux FHS)
    // to the dedicated path; PairOnly falls through to the shared re-pair handler
    // (it writes pairing.json, which is harmless on macOS).
    #[cfg(target_os = "macos")]
    match mode {
        RunMode::Uninstall => return ados_installer::macos::uninstall(args.force),
        RunMode::Status => return ados_installer::macos::status(&args),
        RunMode::FreshInstall | RunMode::Upgrade | RunMode::ForceReinstall => {
            return ados_installer::macos::run(&args)
        }
        _ => {}
    }

    match mode {
        RunMode::Status => {
            status::print_status(&args);
            Ok(ExitCode::SUCCESS)
        }
        RunMode::Uninstall => {
            // `--force` doubles as the purge flag here (remove /etc/ados too)
            // so a `--uninstall --force` wipes identity for a from-clean reinstall.
            let purge = args.force;
            // Drive the same full-screen (or fallback) progress UI the install
            // uses, over the uninstall group set.
            let base = progress::detect_mode(render_flags(&args));
            let theme = Theme::detect(args.no_color, args.ascii);
            let (render_mode, tty) = progress::resolve_live_mode(base, None, Tty::open_controlling);
            let header = "Removing the ADOS Drone Agent…".to_string();
            let (sink, render) =
                progress::start(render_mode, header, theme, tty, UNINSTALL_SPEC, false);
            let res = uninstall::run_uninstall(purge, &sink);
            sink.finish();
            render.finish();
            match res {
                Ok(()) => {
                    println!(
                        "{} ADOS Drone Agent removed{}",
                        theme.ok(theme.glyph_ok()),
                        if purge { " (config purged)" } else { "" }
                    );
                    Ok(ExitCode::SUCCESS)
                }
                Err(e) => {
                    eprintln!("uninstall: {e}");
                    Ok(ExitCode::from(1))
                }
            }
        }
        RunMode::PairOnly => match pair_only::run(&args) {
            Ok(()) => Ok(ExitCode::SUCCESS),
            Err(e) => {
                eprintln!("pair: {e}");
                Ok(ExitCode::from(1))
            }
        },
        RunMode::FreshInstall | RunMode::Upgrade | RunMode::ForceReinstall => {
            run_install(args, mode)
        }
    }
}

/// Drive the install step chain and write the result contract.
fn run_install(mut args: Args, mode: RunMode) -> Result<ExitCode> {
    // A revision pin is only meaningful on the channel that has commit
    // addressing. Refused here, before the wizard, the UI, and any network
    // work, so a contradictory invocation costs nothing and says why.
    if let Some(conflict) = rev_channel_conflict(
        args.rev.as_deref(),
        &resolve_channel(args.channel.as_deref()),
    ) {
        eprintln!("error: {conflict}");
        return Ok(ExitCode::from(2));
    }

    // A local artifact directory is refused on the channel that would reject
    // every file in it for a signature it cannot have, and alongside the pin it
    // would contradict. Same placement, same reason: it costs nothing here and
    // names the real cause instead of surfacing as a per-binary failure fifteen
    // steps later.
    if let Some(conflict) = artifacts_conflict(
        args.artifacts.as_deref(),
        args.rev.as_deref(),
        &resolve_channel(args.channel.as_deref()),
    ) {
        eprintln!("error: {conflict}");
        return Ok(ExitCode::from(2));
    }

    // An explicit `--display <id>` that the detected board does not declare has
    // no binding to apply, so it silently produces a node with no display.
    // Refused here, before any work, against the ids the board's own profile
    // declares (empty = unknown board, which cannot validate and so accepts).
    if let Some(display) = args.display.as_deref() {
        let declared = ados_hal_probe::board_sidecar::declared_display_ids();
        if let Some(err) = ados_installer::cli::display_value_error(display, &declared) {
            eprintln!("error: {err}");
            return Ok(ExitCode::from(2));
        }
    }

    // Interactive onboarding: only on a fresh, interactive install where the
    // operator has not already pinned the answers with flags. It collects the
    // choices into `args` + `wizard_extras`, then the install proceeds exactly
    // as the flag-driven path would. When the terminal is unavailable or a
    // decisive flag was passed, the wizard stays out and the silent
    // auto-detected install runs unchanged (a fresh box still comes up with
    // zero follow-up commands).
    let mut wizard_extras: Option<wizard::WizardExtras> = None;
    // The wizard hands back its still-open `/dev/tty` on completion so the
    // install progress renders in the SAME alternate-screen session.
    let mut carried_tty: Option<Tty> = None;
    if mode == RunMode::FreshInstall && wizard::should_run(&args) {
        match wizard::run(&mut args) {
            Ok((WizardControl::Completed(extras), tty)) => {
                wizard_extras = Some(extras);
                carried_tty = tty;
            }
            Ok((WizardControl::Skipped, _)) => {}
            Ok((WizardControl::Canceled, _)) => {
                eprintln!("Setup canceled.");
                return Ok(ExitCode::from(130));
            }
            Err(e) => {
                tracing::warn!(error = %e, "onboarding wizard failed; continuing non-interactive")
            }
        }
    }

    // Start the live progress UI before any work so the operator sees feedback
    // immediately. The base mode is chosen from stderr + the environment;
    // `resolve_live_mode` upgrades it to the full-screen renderer when a
    // controlling terminal is reachable (a carried wizard tty, or a fresh open).
    let base_mode = progress::detect_mode(render_flags(&args));
    let theme = Theme::detect(args.no_color, args.ascii);
    // The onboarding wizard ran (and carried its tty) only on an interactive
    // install; that gates the full-screen "installed" completion + key wait so
    // an automated install never blocks. Capture it before the tty is consumed.
    let interactive = carried_tty.is_some();
    let (render_mode, live_tty) =
        progress::resolve_live_mode(base_mode, carried_tty, Tty::open_controlling);
    let profile_hint = args.profile.clone().unwrap_or_else(|| "drone".to_string());
    let header = format!("Installing the ADOS Drone Agent ({profile_hint})…");
    let (sink, render) = progress::start(
        render_mode,
        header,
        theme,
        live_tty,
        INSTALL_SPEC,
        interactive,
    );

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
    // Carry the wizard's region + cloud-posture choices into the config step.
    if let Some(extras) = wizard_extras {
        ctx.region_pinned = extras.region_pinned;
        ctx.cloud_from_anywhere = extras.cloud_from_anywhere;
    }

    let reports = run_graph(full_install_chain(), &mut ctx);
    let status = ctx.failures.derive_status();
    tracing::info!(
        status,
        steps = reports.len(),
        failed = ctx.failures.failed.len(),
        required = ctx.failures.required.len(),
        "install finished"
    );

    // A required step failed after the fetch swapped binaries in: put every
    // binary this run replaced back and restart the units that run them, so the
    // node is left on the build it was running instead of a half-applied
    // upgrade. The exit stays non-zero below.
    if status == "failed" && !ctx.replaced_binaries.is_empty() {
        let report = ados_installer::rollback::roll_back(&ctx.replaced_binaries);
        tracing::error!(
            restored = report.restored.len(),
            no_previous = report.no_previous.len(),
            failed = report.failed.len(),
            restarted = %report.restarted.join(","),
            "install failed after replacing binaries; rolled back: {}",
            report.summary()
        );
        sink.sub_log(
            "fetch_binaries",
            &format!("Rolled back service binaries: {}", report.summary()),
        );
    }

    // Build + write the result contract (best-effort: a dev host where
    // /var/lib is not writable must not panic the binary). The profile was
    // resolved into ctx by preflight, so read it back from there.
    report::write_result(&ctx.failures, status, &ctx.profile);

    // Keep this installer on the box so `ados uninstall` can run the one
    // uninstall path offline. Only after an install that did not fail: a
    // failed run leaves the previous copy in place.
    if status != "failed" {
        report::persist_installer();
    }

    // Hand the renderer the closing summary, then wait for it to draw the
    // success card / failure panel and restore the terminal.
    if render_mode == RenderMode::Json {
        report::print_result_json();
    }
    sink.summary(Box::new(report::build_summary(status, &ctx)));
    sink.finish();
    render.finish();

    // The single automatic reboot, AFTER the closing card is drawn and the
    // terminal restored — so the operator reads what is staged and why the box
    // is about to go away, instead of the screen dying mid-render. `--no-reboot`
    // defers it; the `reboot` step has already degraded the install and named
    // the staged provisioning in the summary above.
    if !ctx.pending_reboot.is_empty() && !ctx.args.no_reboot && status != "failed" {
        println!(
            "Rebooting to finish provisioning: {}",
            ctx.pending_reboot.join(", ")
        );
        if !exec::run_ok("systemctl", &["reboot"]) {
            // Nothing left to fall back on, so say so plainly rather than
            // exiting as if the box were in its provisioned state.
            println!("Automatic reboot failed; reboot this node to finish provisioning.");
        }
    }

    if status == "failed" {
        Ok(ExitCode::from(1))
    } else {
        Ok(ExitCode::SUCCESS)
    }
}
