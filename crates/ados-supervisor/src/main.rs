//! `ados-supervisor` binary: a thin shim over [`ados_supervisor::daemon::run`],
//! or, as `ados-supervisor switch-profile`, the one-shot that moves the node's
//! units onto the profile its config names ([`ados_supervisor::profile_switch`]).

use std::process::ExitCode;

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        None => ados_supervisor::daemon::run(),
        Some("switch-profile") => ados_supervisor::profile_switch::run(),
        Some(other) => {
            eprintln!("usage: ados-supervisor [switch-profile] (unknown argument {other:?})");
            ExitCode::from(64)
        }
    }
}
