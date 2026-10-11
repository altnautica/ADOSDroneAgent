//! `ados-supervisor` binary: a thin shim over [`ados_supervisor::daemon::run`].

use std::process::ExitCode;

fn main() -> ExitCode {
    ados_supervisor::daemon::run()
}
