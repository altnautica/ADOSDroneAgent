//! `ados-mavlink-router` binary: a thin entry point into
//! [`ados_mavlink_router::daemon::run`].

use std::process::ExitCode;

fn main() -> ExitCode {
    ados_mavlink_router::daemon::run()
}
