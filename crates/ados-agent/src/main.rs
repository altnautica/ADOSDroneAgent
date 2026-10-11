//! `ados-agent`: one executable carrying every Rust daemon of the agent.
//!
//! The applet is chosen by the basename of `argv[0]`, so a hardlink or
//! symlink named `ados-video` runs the video daemon with its own argument
//! list untouched. Invoked as `ados-agent <applet> [args...]` it re-executes
//! itself under the applet name (Unix), so the daemon sees exactly the argv it
//! would see as a standalone binary. An unknown applet prints the usage and
//! exits 64 (`EX_USAGE`).

use std::ffi::OsString;
use std::path::Path;
use std::process::ExitCode;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// The multi-call binary's own name.
const SELF_NAME: &str = "ados-agent";

/// `EX_USAGE` from sysexits.h.
const EX_USAGE: u8 = 64;

type Applet = fn() -> ExitCode;

/// Every Rust daemon, keyed by the binary name its unit execs.
const APPLETS: &[(&str, Applet)] = &[
    ("ados-camera-probe", ados_hal_probe::daemon::run),
    ("ados-cloud", ados_cloud::daemon::run),
    ("ados-control", ados_control::daemon::run),
    ("ados-crsf", ados_crsf::daemon::run),
    ("ados-display", ados_display::daemon::run_display),
    (
        "ados-display-probe",
        ados_display::daemon::run_display_probe,
    ),
    #[cfg(unix)]
    ("ados-gpio", ados_gpio::daemon::run),
    #[cfg(unix)]
    ("ados-groundlink", ados_groundlink::daemon::run),
    #[cfg(unix)]
    ("ados-input", ados_hid::daemon::run_input),
    ("ados-logd", ados_logd::daemon::run),
    ("ados-mavlink-router", ados_mavlink_router::daemon::run),
    #[cfg(unix)]
    ("ados-net", ados_net::daemon::run),
    ("ados-oled-i2c", ados_display::daemon::run_oled_i2c),
    #[cfg(unix)]
    ("ados-pic", ados_hid::daemon::run_pic),
    #[cfg(unix)]
    ("ados-plugin-host", ados_plugin_host::daemon::run),
    ("ados-radio", ados_radio::daemon::run),
    #[cfg(unix)]
    ("ados-supervisor", ados_supervisor::daemon::run),
    ("ados-swarmbus", ados_swarmbus::daemon::run),
    ("ados-tunnel-config", ados_config_tunnel::daemon::run),
    ("ados-video", ados_video::daemon::run),
];

/// What the command line asks for.
#[derive(Debug, PartialEq)]
enum Dispatch {
    /// `argv[0]` names an applet: run it in this process.
    Run(&'static str),
    /// `ados-agent <applet> ...`: run the applet with argv shifted by one.
    Shifted(&'static str),
    /// No applet matched; the payload is the name that was asked for.
    Unknown(String),
}

/// Find an applet by its binary name, also accepting the short form without
/// the `ados-` prefix (`ados-agent video`).
fn lookup(name: &str) -> Option<(&'static str, Applet)> {
    let full = if name.starts_with("ados-") {
        name.to_owned()
    } else {
        format!("ados-{name}")
    };
    APPLETS
        .iter()
        .find(|(n, _)| *n == full)
        .map(|(n, f)| (*n, *f))
}

fn basename(arg0: &OsString) -> String {
    let base = Path::new(arg0)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    base.strip_suffix(".exe").map(str::to_owned).unwrap_or(base)
}

fn resolve(args: &[OsString]) -> Dispatch {
    let invoked = args.first().map(basename).unwrap_or_default();
    if invoked != SELF_NAME {
        return match APPLETS.iter().find(|(n, _)| *n == invoked) {
            Some((n, _)) => Dispatch::Run(n),
            None => Dispatch::Unknown(invoked),
        };
    }
    let Some(requested) = args.get(1).map(|a| a.to_string_lossy().into_owned()) else {
        return Dispatch::Unknown(String::new());
    };
    match lookup(&requested) {
        Some((n, _)) => Dispatch::Shifted(n),
        None => Dispatch::Unknown(requested),
    }
}

fn usage(requested: &str) -> ExitCode {
    if !requested.is_empty() {
        eprintln!("{SELF_NAME}: unknown applet '{requested}'");
    }
    eprintln!(
        "usage: {SELF_NAME} <applet> [args...]   (or invoke via a link named after the applet)"
    );
    eprintln!("applets:");
    for (name, _) in APPLETS {
        eprintln!("  {name}");
    }
    ExitCode::from(EX_USAGE)
}

/// Run `name` as if it had been exec'd directly: on Unix, re-exec this binary
/// with `argv[0]` set to the applet name and the remaining arguments, so the
/// daemon's own argument parsing sees its usual argv.
#[cfg(unix)]
fn run_shifted(name: &str, rest: &[OsString]) -> ExitCode {
    use std::os::unix::process::CommandExt;
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{SELF_NAME}: cannot locate own executable: {e}");
            return ExitCode::FAILURE;
        }
    };
    let err = std::process::Command::new(exe).arg0(name).args(rest).exec();
    eprintln!("{SELF_NAME}: exec {name} failed: {err}");
    ExitCode::FAILURE
}

#[cfg(not(unix))]
fn run_shifted(name: &str, _rest: &[OsString]) -> ExitCode {
    match lookup(name) {
        Some((_, run)) => run(),
        None => usage(name),
    }
}

fn dispatch(args: &[OsString]) -> ExitCode {
    match resolve(args) {
        Dispatch::Run(name) => match lookup(name) {
            Some((_, run)) => run(),
            None => usage(name),
        },
        Dispatch::Shifted(name) => run_shifted(name, args.get(2..).unwrap_or_default()),
        Dispatch::Unknown(requested) => usage(&requested),
    }
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    dispatch(&args)
}

#[cfg(test)]
mod tests;
