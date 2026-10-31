//! The install-result record and the closing operator summary.

use ados_term::exec;

use crate::result::{now_iso8601_utc, InstallResult};

use super::launchd::teardown_daemon_names;
use super::{HealthReport, Paths, CONTROL_PORT};

/// Write the install-result contract to `$HOME/.ados/install-result.json` so the
/// control surface serves the real version and `ados status` reports the outcome.
/// Health failures are recorded as required failures so the status is `failed`
/// when a daemon did not come up.
pub(super) fn write_macos_install_result(
    paths: &Paths,
    profile: &str,
    version: &str,
    report: &HealthReport,
) {
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
pub(super) fn print_summary(paths: &Paths, device_id: &str, uid: u32, report: &HealthReport) {
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
    println!(
        "    for p in {}/co.ados.plugin.*.plist; do [ -e \"$p\" ] && launchctl bootout gui/{uid}/$(basename \"$p\" .plist); done",
        paths.launch_agents.display()
    );
    println!(
        "    for s in {}; do launchctl bootout gui/{uid}/co.ados.$s 2>/dev/null; done",
        teardown_daemon_names()
    );
    println!("    rm {}/co.ados.*.plist", paths.launch_agents.display());
    println!("    rm -rf {}", paths.ados_home.display());
}

// ── small helpers ────────────────────────────────────────────────────────────

/// This host's LAN IPv4 (`en0`, then `en1`), or `127.0.0.1`. Used for the
/// operator reach block.
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
