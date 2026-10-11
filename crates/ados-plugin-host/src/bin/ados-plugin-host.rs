//! `ados-plugin-host` binary: a thin entry into [`ados_plugin_host::daemon::run`].

use std::process::ExitCode;

fn main() -> ExitCode {
    ados_plugin_host::daemon::run()
}
