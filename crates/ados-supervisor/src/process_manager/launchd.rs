//! launchd backend: drives service jobs via the `launchctl` binary.
//!
//! Maps the supervisor's lifecycle verbs onto the modern `launchctl` subcommand
//! surface in the GUI domain of the running user (`gui/<uid>/<label>`). A unit
//! name like `ados-compute.service` maps to the reverse-DNS launchd label
//! `co.ados.compute`. launchd has no direct `reset-failed` analogue, so that
//! verb is a documented best-effort no-op. A missing `launchctl` or a timeout is
//! a soft failure, matching the systemd backend.

use std::time::Duration;

use async_trait::async_trait;
use tokio::process::Command;
use tokio::time::timeout;

use super::ProcessManager;

const ACT_TIMEOUT: Duration = Duration::from_secs(30);
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Drives service jobs via the `launchctl` binary on macOS.
pub struct LaunchdManager;

/// Map a systemd-style unit name to a launchd reverse-DNS label.
///
/// Strips a trailing `.service` and rewrites an `ados-` prefix to the `co.ados.`
/// domain, so `ados-compute.service` becomes `co.ados.compute`. A name without
/// the `ados-` prefix is used as-is (minus the suffix).
pub fn unit_to_label(unit: &str) -> String {
    let base = unit.strip_suffix(".service").unwrap_or(unit);
    match base.strip_prefix("ados-") {
        Some(rest) => format!("co.ados.{rest}"),
        None => base.to_string(),
    }
}

/// Effective user id for the launchd GUI domain target. Read from `id -u` so the
/// backend needs no platform-gated libc/nix dependency and compiles on every
/// host the selector may construct it on; falls back to 0 when unreadable.
async fn current_uid() -> u32 {
    match timeout(PROBE_TIMEOUT, Command::new("id").arg("-u").output()).await {
        Ok(Ok(out)) => String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse::<u32>()
            .unwrap_or(0),
        _ => 0,
    }
}

/// The `gui/<uid>/<label>` service target the modern `launchctl` verbs address.
async fn service_target(unit: &str) -> String {
    format!("gui/{}/{}", current_uid().await, unit_to_label(unit))
}

async fn run(args: &[&str], dur: Duration) -> Option<std::process::Output> {
    match timeout(dur, Command::new("launchctl").args(args).output()).await {
        Ok(Ok(out)) => Some(out),
        Ok(Err(_)) => None, // spawn error (launchctl missing)
        Err(_) => None,     // timed out
    }
}

fn ok(out: &Option<std::process::Output>) -> bool {
    out.as_ref().map(|o| o.status.success()).unwrap_or(false)
}

#[async_trait]
impl ProcessManager for LaunchdManager {
    /// `launchctl kickstart -k gui/<uid>/<label>` — start (and replace any
    /// running instance of) the job.
    async fn start(&self, unit: &str) -> bool {
        let target = service_target(unit).await;
        ok(&run(&["kickstart", "-k", &target], ACT_TIMEOUT).await)
    }

    /// `launchctl bootout gui/<uid>/<label>` — remove the job from the domain.
    async fn stop(&self, unit: &str) -> bool {
        let target = service_target(unit).await;
        ok(&run(&["bootout", &target], ACT_TIMEOUT).await)
    }

    /// `launchctl kickstart -k gui/<uid>/<label>` — `-k` forces a fresh spawn
    /// cycle by killing any running instance first, the restart equivalent.
    async fn restart(&self, unit: &str) -> bool {
        let target = service_target(unit).await;
        ok(&run(&["kickstart", "-k", &target], ACT_TIMEOUT).await)
    }

    /// No-op: launchd has no `reset-failed` analogue. `kickstart -k` already
    /// forces a restart regardless of the prior exit state, so there is no
    /// failed-burst counter to clear before a start.
    async fn reset_failed(&self, _unit: &str) {}

    /// True when `launchctl print gui/<uid>/<label>` reports `state = running`.
    async fn is_active(&self, unit: &str) -> bool {
        let target = service_target(unit).await;
        match run(&["print", &target], PROBE_TIMEOUT).await {
            Some(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
                .lines()
                .any(|l| l.trim() == "state = running"),
            _ => false,
        }
    }

    /// `launchctl disable gui/<uid>/<label>` (idempotent).
    async fn mask(&self, unit: &str) {
        let target = service_target(unit).await;
        let _ = run(&["disable", &target], PROBE_TIMEOUT).await;
    }

    /// `launchctl enable gui/<uid>/<label>` (idempotent).
    async fn unmask(&self, unit: &str) {
        let target = service_target(unit).await;
        let _ = run(&["enable", &target], PROBE_TIMEOUT).await;
    }
}

/// XML-escape a string for inclusion in a plist `<string>` / `<key>` element.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Optional job log redirection for a rendered plist. launchd defaults a job's
/// stdout/stderr to `/dev/null`; pointing them at a file is what makes a
/// background agent inspectable after the fact.
#[derive(Debug, Clone, Copy, Default)]
pub struct PlistLogPaths<'a> {
    /// Absolute path for the job's stdout (`StandardOutPath`), or `None`.
    pub stdout: Option<&'a str>,
    /// Absolute path for the job's stderr (`StandardErrorPath`), or `None`.
    pub stderr: Option<&'a str>,
}

/// Render a launchd property list for a managed service.
///
/// Emits `ProgramArguments` (the `program` as argv[0] followed by `args`),
/// `EnvironmentVariables` (when any are given), `StandardOutPath` /
/// `StandardErrorPath` (when a log path is given), `RunAtLoad` true, and — when
/// `keep_alive` — a `KeepAlive` dict requesting a restart only on a crash
/// (`Crashed` true). The output is a complete, well-formed plist document.
pub fn render_plist(
    label: &str,
    program: &str,
    args: &[String],
    env: &[(String, String)],
    keep_alive: bool,
    logs: PlistLogPaths<'_>,
) -> String {
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str(
        "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
    );
    out.push_str("<plist version=\"1.0\">\n");
    out.push_str("<dict>\n");

    out.push_str("\t<key>Label</key>\n");
    out.push_str(&format!("\t<string>{}</string>\n", xml_escape(label)));

    out.push_str("\t<key>ProgramArguments</key>\n");
    out.push_str("\t<array>\n");
    out.push_str(&format!("\t\t<string>{}</string>\n", xml_escape(program)));
    for a in args {
        out.push_str(&format!("\t\t<string>{}</string>\n", xml_escape(a)));
    }
    out.push_str("\t</array>\n");

    if !env.is_empty() {
        out.push_str("\t<key>EnvironmentVariables</key>\n");
        out.push_str("\t<dict>\n");
        for (k, v) in env {
            out.push_str(&format!("\t\t<key>{}</key>\n", xml_escape(k)));
            out.push_str(&format!("\t\t<string>{}</string>\n", xml_escape(v)));
        }
        out.push_str("\t</dict>\n");
    }

    if let Some(path) = logs.stdout {
        out.push_str("\t<key>StandardOutPath</key>\n");
        out.push_str(&format!("\t<string>{}</string>\n", xml_escape(path)));
    }
    if let Some(path) = logs.stderr {
        out.push_str("\t<key>StandardErrorPath</key>\n");
        out.push_str(&format!("\t<string>{}</string>\n", xml_escape(path)));
    }

    out.push_str("\t<key>RunAtLoad</key>\n");
    out.push_str("\t<true/>\n");

    if keep_alive {
        out.push_str("\t<key>KeepAlive</key>\n");
        out.push_str("\t<dict>\n");
        out.push_str("\t\t<key>Crashed</key>\n");
        out.push_str("\t\t<true/>\n");
        out.push_str("\t</dict>\n");
    }

    out.push_str("</dict>\n");
    out.push_str("</plist>\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_to_label_strips_suffix_and_maps_prefix() {
        assert_eq!(unit_to_label("ados-compute.service"), "co.ados.compute");
        assert_eq!(unit_to_label("ados-wfb"), "co.ados.wfb");
        // A multi-segment service tail is preserved after the domain rewrite.
        assert_eq!(unit_to_label("ados-wfb-rx.service"), "co.ados.wfb-rx");
        // A name without the ados- prefix is used as-is (suffix still stripped).
        assert_eq!(unit_to_label("mediamtx.service"), "mediamtx");
        assert_eq!(unit_to_label("custom"), "custom");
    }

    #[test]
    fn render_plist_emits_program_args_env_and_keepalive() {
        let plist = render_plist(
            "co.ados.compute",
            "/opt/ados/bin/ados-compute",
            &["--profile".to_string(), "compute".to_string()],
            &[("ADOS_HOME".to_string(), "/var/ados".to_string())],
            true,
            PlistLogPaths::default(),
        );
        // Document framing.
        assert!(plist.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
        assert!(plist.contains("<plist version=\"1.0\">"));
        assert!(plist.trim_end().ends_with("</plist>"));
        // Label + program argv[0] + args in order.
        assert!(plist.contains("<key>Label</key>"));
        assert!(plist.contains("<string>co.ados.compute</string>"));
        assert!(plist.contains("<key>ProgramArguments</key>"));
        assert!(plist.contains("<string>/opt/ados/bin/ados-compute</string>"));
        assert!(plist.contains("<string>--profile</string>"));
        assert!(plist.contains("<string>compute</string>"));
        // Environment variables dict.
        assert!(plist.contains("<key>EnvironmentVariables</key>"));
        assert!(plist.contains("<key>ADOS_HOME</key>"));
        assert!(plist.contains("<string>/var/ados</string>"));
        // RunAtLoad true + KeepAlive {Crashed: true}.
        assert!(plist.contains("<key>RunAtLoad</key>"));
        assert!(plist.contains("<key>KeepAlive</key>"));
        assert!(plist.contains("<key>Crashed</key>"));
    }

    #[test]
    fn render_plist_omits_keepalive_and_env_when_unset() {
        let plist = render_plist(
            "co.ados.logd",
            "/opt/ados/bin/ados-logd",
            &[],
            &[],
            false,
            PlistLogPaths::default(),
        );
        assert!(!plist.contains("KeepAlive"));
        assert!(!plist.contains("EnvironmentVariables"));
        // No log paths were given, so neither redirection key is emitted.
        assert!(!plist.contains("StandardOutPath"));
        assert!(!plist.contains("StandardErrorPath"));
        // The program is still the sole ProgramArguments entry.
        assert!(plist.contains("<string>/opt/ados/bin/ados-logd</string>"));
        assert!(plist.contains("<key>RunAtLoad</key>"));
    }

    #[test]
    fn render_plist_emits_log_redirection_when_given() {
        let plist = render_plist(
            "co.ados.control",
            "/Users/op/.ados/bin/ados-control",
            &[],
            &[],
            true,
            PlistLogPaths {
                stdout: Some("/Users/op/.ados/log/control.out.log"),
                stderr: Some("/Users/op/.ados/log/control.err.log"),
            },
        );
        assert!(plist.contains("<key>StandardOutPath</key>"));
        assert!(plist.contains("<string>/Users/op/.ados/log/control.out.log</string>"));
        assert!(plist.contains("<key>StandardErrorPath</key>"));
        assert!(plist.contains("<string>/Users/op/.ados/log/control.err.log</string>"));
    }

    #[test]
    fn render_plist_escapes_xml_special_characters() {
        let plist = render_plist(
            "co.ados.x",
            "/bin/sh",
            &["a < b & c > d".to_string()],
            &[("K".to_string(), "v&<>".to_string())],
            false,
            PlistLogPaths::default(),
        );
        assert!(plist.contains("a &lt; b &amp; c &gt; d"));
        assert!(plist.contains("v&amp;&lt;&gt;"));
        // No raw special characters leaked into the rendered argument value.
        assert!(!plist.contains("a < b & c > d"));
    }
}
