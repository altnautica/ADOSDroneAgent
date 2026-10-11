//! `ados-swarmbus`: the daemon body lives in the library (`ados_swarmbus::daemon::run`).

fn main() -> std::process::ExitCode {
    ados_swarmbus::daemon::run()
}
