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
use ados_installer::wizard::{self, WizardControl};

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

    // Probe whether the agent is already installed on this box (venv + deployed
    // supervisor unit + persisted identity). A bare `--pair CODE` against an
    // existing install resolves to a fast re-pair; on a fresh box the same
    // invocation runs the full install (which pairs at the end).
    let already_installed = env::probe_install_present();
    let mode = RunMode::resolve(&args, already_installed);
    tracing::info!(?mode, already_installed, "resolved install run-mode");

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
            print_status(&args);
            Ok(ExitCode::SUCCESS)
        }
        RunMode::Uninstall => {
            // `--force` doubles as the purge flag here (remove /etc/ados too)
            // so a `--uninstall --force` wipes identity for a from-clean reinstall.
            let purge = args.force;
            // Drive the same full-screen (or fallback) progress UI the install
            // uses, over the uninstall group set.
            let base = ui::detect_mode(&args);
            let theme = ui::Theme::detect(args.no_color, args.ascii);
            let (render_mode, tty) = ui::resolve_live_mode(base, None);
            let header = "Removing the ADOS Drone Agent…".to_string();
            let (sink, render) = ui::start(
                render_mode,
                header,
                theme,
                tty,
                ui::UNINSTALL_GROUPS,
                ui::UNINSTALL_FOOTER,
                false,
            );
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
        RunMode::PairOnly => match run_pair_only(&args) {
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
    let mut carried_tty: Option<ui::tty::Tty> = None;
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
    let base_mode = ui::detect_mode(&args);
    let theme = ui::Theme::detect(args.no_color, args.ascii);
    // The onboarding wizard ran (and carried its tty) only on an interactive
    // install; that gates the full-screen "installed" completion + key wait so
    // an automated install never blocks. Capture it before the tty is consumed.
    let interactive = carried_tty.is_some();
    let (render_mode, live_tty) = ui::resolve_live_mode(base_mode, carried_tty);
    let profile_hint = args.profile.clone().unwrap_or_else(|| "drone".to_string());
    let header = format!("Installing the ADOS Drone Agent ({profile_hint})…");
    let (sink, render) = ui::start(
        render_mode,
        header,
        theme,
        live_tty,
        ui::INSTALL_GROUPS,
        ui::INSTALL_FOOTER,
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

/// Fast re-pair against an already-installed, running agent: write fresh
/// pairing material and nudge the agent to pick it up, without the full install
/// chain (no re-fetch, no re-provision). Reached only when the install-presence
/// probe confirmed the agent is on disk and a bare `--pair CODE` was given.
///
/// The agent auto-reloads `pairing.json` when its on-disk mtime is newer than
/// the in-memory copy, so writing a fresh file is sufficient for correctness;
/// restarting the cloud-relay unit forces an immediate re-read and re-beacon so
/// the operator sees the new code take effect at once rather than on the next
/// reload tick. The restart is best-effort: a write that lands but a restart
/// that fails still re-pairs on the agent's own reload.
fn run_pair_only(args: &Args) -> Result<()> {
    let code = validate_pair_code(args.pair.as_deref())?;

    write_pairing_material(&code)?;
    nudge_cloud_relay();

    println!(
        "pair: re-paired the running agent with code {}",
        code.to_ascii_uppercase()
    );
    Ok(())
}

/// Validate the supplied pairing code (pure): trim surrounding whitespace and
/// reject an absent or blank code. Returns the trimmed code on success.
fn validate_pair_code(pair: Option<&str>) -> Result<String> {
    match pair.map(str::trim) {
        Some(c) if !c.is_empty() => Ok(c.to_string()),
        _ => anyhow::bail!("no pairing code supplied"),
    }
}

/// Write `/etc/ados/pairing.json` with the supplied code (uppercased, stamped),
/// reusing the same body builder the install's config step uses so the on-disk
/// shape never drifts. Mode 0600 — the file carries pairing identity.
fn write_pairing_material(code: &str) -> Result<()> {
    use ados_installer::steps::config_identity::pairing_json;
    let path = Path::new(env::PAIRING_JSON);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow::anyhow!("create {} failed: {e}", parent.display()))?;
    }
    let body = pairing_json(code, now_epoch());
    std::fs::write(path, body)
        .map_err(|e| anyhow::anyhow!("write {} failed: {e}", env::PAIRING_JSON))?;
    set_pairing_mode(path);
    tracing::info!(code = %code.to_ascii_uppercase(), "pairing material rewritten for re-pair");
    Ok(())
}

/// Restart the cloud unit that beacons the pair code so the new code is re-read
/// immediately. The single `ados-cloud` unit serves both profiles — it spawns
/// the ground-station bridge when the role resolves to a ground station — so one
/// unit covers every box. Only an installed-and-active unit restarts; otherwise
/// this is a harmless no-op. Best-effort by design — see [`run_pair_only`].
fn nudge_cloud_relay() {
    const UNIT: &str = "ados-cloud";
    // Only restart a unit that is actually active so we never spuriously start a
    // unit that has not come up yet on this box.
    if exec::run_ok("systemctl", &["is-active", "--quiet", UNIT]) {
        if exec::run_ok("systemctl", &["restart", UNIT]) {
            tracing::info!(
                unit = UNIT,
                "restarted cloud relay to re-beacon the new pair code"
            );
        } else {
            tracing::warn!(
                unit = UNIT,
                "cloud relay restart failed; the agent will re-read pairing.json on its next reload"
            );
        }
    }
}

/// Seconds since the Unix epoch (for the pairing stamp).
fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// chmod the pairing file to 0600 on Unix; a no-op on a non-Unix dev host.
#[cfg(unix)]
fn set_pairing_mode(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_pairing_mode(_path: &Path) {}

/// Assemble the renderer's closing-summary payload from the resolved context.
fn build_summary(status: &str, ctx: &Ctx) -> ui::SummaryData {
    let hostname = env::read_hostname();
    let setup_url = format!("http://{hostname}.local:8080/setup");
    ui::SummaryData {
        status: status.to_string(),
        version: installed_version(),
        profile: ctx.profile.clone(),
        board: board_id(),
        device_id: read_device_id(),
        hostname,
        setup_url,
        lan_ips: probe_lan_ips(),
        paired: pairing_present(),
        failed_steps: ctx.failures.failed.clone(),
        required_failures: ctx.failures.required.clone(),
    }
}

/// The box's non-loopback IPv4 addresses, for the success-card reach block.
/// Prefers `ip -o -4 addr show` (per-interface, so bridge/container NICs are
/// filtered by name) and falls back to `hostname -I`. Both are parsed by the
/// pure helpers below; the returned list preserves discovery order and is
/// de-duplicated. Empty when the box has no routable IPv4 (the card then leads
/// with the `<host>.local` mDNS name alone).
fn probe_lan_ips() -> Vec<String> {
    let res = exec::run("ip", &["-o", "-4", "addr", "show"]);
    if res.success() {
        let ips = parse_ip_o_4(&res.stdout);
        if !ips.is_empty() {
            return ips;
        }
    }
    let res = exec::run("hostname", &["-I"]);
    if res.success() {
        return parse_hostname_i(&res.stdout);
    }
    Vec::new()
}

/// Parse `ip -o -4 addr show` output into a de-duplicated list of routable
/// IPv4 addresses, skipping loopback and virtual (bridge/container/VPN)
/// interfaces by name. Each line looks like:
/// `2: eth0    inet 192.168.1.42/24 brd ... scope global eth0\ ...`.
fn parse_ip_o_4(output: &str) -> Vec<String> {
    let mut ips = Vec::new();
    for line in output.lines() {
        let mut fields = line.split_whitespace();
        // fields: <idx:> <iface> ... inet <ip/cidr> ...
        let _idx = fields.next();
        let iface = match fields.next() {
            Some(i) => i,
            None => continue,
        };
        if is_virtual_iface(iface) {
            continue;
        }
        // Find the `inet` token, take the following `ip/cidr`.
        let mut rest = fields;
        while let Some(tok) = rest.next() {
            if tok == "inet" {
                if let Some(cidr) = rest.next() {
                    let ip = cidr.split('/').next().unwrap_or("");
                    push_routable_ipv4(&mut ips, ip);
                }
                break;
            }
        }
    }
    ips
}

/// Parse `hostname -I` output (a space-separated address list, IPv4 + IPv6)
/// into a de-duplicated list of routable IPv4 addresses. No interface names are
/// available here, so filtering is by address form only.
fn parse_hostname_i(output: &str) -> Vec<String> {
    let mut ips = Vec::new();
    for tok in output.split_whitespace() {
        push_routable_ipv4(&mut ips, tok);
    }
    ips
}

/// Push `ip` onto `ips` when it is a routable (non-loopback, non-empty) IPv4
/// dotted-quad and not already present.
fn push_routable_ipv4(ips: &mut Vec<String>, ip: &str) {
    if is_routable_ipv4(ip) && !ips.iter().any(|existing| existing == ip) {
        ips.push(ip.to_string());
    }
}

/// True for a dotted-quad IPv4 that is neither empty nor loopback (`127.x`).
fn is_routable_ipv4(ip: &str) -> bool {
    if ip.is_empty() || ip.starts_with("127.") {
        return false;
    }
    let octets: Vec<&str> = ip.split('.').collect();
    octets.len() == 4
        && octets
            .iter()
            .all(|o| !o.is_empty() && o.len() <= 3 && o.chars().all(|c| c.is_ascii_digit()))
}

/// True for interface names that are loopback or virtual (bridge, container,
/// VPN, mesh) and should not appear in the operator's reach block.
fn is_virtual_iface(iface: &str) -> bool {
    const VIRTUAL_PREFIXES: &[&str] = &[
        "lo",
        "docker",
        "br-",
        "veth",
        "virbr",
        "tailscale",
        "zt",
        "cni",
        "flannel",
        "kube",
        "podman",
        "cali",
        "wg",
    ];
    VIRTUAL_PREFIXES.iter().any(|p| iface.starts_with(p))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_pair_code_trims_and_rejects_blank() {
        // A real code is accepted, trimmed of surrounding whitespace.
        assert_eq!(
            validate_pair_code(Some("  ABCD-1234 ")).unwrap(),
            "ABCD-1234"
        );
        assert_eq!(validate_pair_code(Some("code")).unwrap(), "code");
        // Absent or blank → an error (a re-pair with no code must not proceed).
        assert!(validate_pair_code(None).is_err());
        assert!(validate_pair_code(Some("")).is_err());
        assert!(validate_pair_code(Some("   ")).is_err());
    }

    #[test]
    fn parse_ip_o_4_keeps_lan_and_drops_loopback_and_virtual() {
        let out = "\
1: lo    inet 127.0.0.1/8 scope host lo\\       valid_lft forever preferred_lft forever
2: end0    inet 192.168.1.42/24 brd 192.168.1.255 scope global end0\\       valid_lft forever
3: docker0    inet 172.17.0.1/16 brd 172.17.255.255 scope global docker0\\       valid_lft forever
4: wlan1    inet 10.0.0.7/24 brd 10.0.0.255 scope global wlan1\\       valid_lft forever
";
        let ips = parse_ip_o_4(out);
        assert_eq!(
            ips,
            vec!["192.168.1.42".to_string(), "10.0.0.7".to_string()]
        );
    }

    #[test]
    fn parse_hostname_i_keeps_ipv4_and_drops_ipv6_and_loopback() {
        // `hostname -I` yields a space-separated mix of v4 + v6; keep only the
        // routable dotted-quads, de-duplicated in order.
        let ips = parse_hostname_i("192.168.1.42 fe80::1 10.0.0.7 192.168.1.42 127.0.0.1\n");
        assert_eq!(
            ips,
            vec!["192.168.1.42".to_string(), "10.0.0.7".to_string()]
        );
    }

    #[test]
    fn is_routable_ipv4_rejects_loopback_and_non_dotted_quads() {
        assert!(is_routable_ipv4("192.168.0.1"));
        assert!(!is_routable_ipv4("127.0.0.1"));
        assert!(!is_routable_ipv4(""));
        assert!(!is_routable_ipv4("fe80::1"));
        assert!(!is_routable_ipv4("192.168.0"));
        assert!(!is_routable_ipv4("1.2.3.4.5"));
    }

    #[test]
    fn is_virtual_iface_flags_bridge_and_container_nics() {
        assert!(is_virtual_iface("lo"));
        assert!(is_virtual_iface("docker0"));
        assert!(is_virtual_iface("br-abc123"));
        assert!(is_virtual_iface("veth9f2"));
        assert!(!is_virtual_iface("eth0"));
        assert!(!is_virtual_iface("end0"));
        assert!(!is_virtual_iface("wlan1"));
    }

    #[test]
    fn pair_only_resolves_only_for_an_installed_box_with_a_bare_code() {
        // A bare pair code resolves to PairOnly only when the install probe says
        // the agent is already on disk; on a fresh box the same code installs.
        let a = Args {
            pair: Some("ABCD-1234".to_string()),
            ..Args::default()
        };
        assert_eq!(RunMode::resolve(&a, true), RunMode::PairOnly);
        assert_eq!(RunMode::resolve(&a, false), RunMode::FreshInstall);
        // --upgrade or --force never short-circuits to a re-pair.
        let up = Args {
            upgrade: true,
            ..a.clone()
        };
        assert_eq!(RunMode::resolve(&up, true), RunMode::Upgrade);
        let force = Args { force: true, ..a };
        assert_eq!(RunMode::resolve(&force, true), RunMode::ForceReinstall);
    }
}
