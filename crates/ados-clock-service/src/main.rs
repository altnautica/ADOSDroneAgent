//! `ados-clock`: the daemon body lives in the library (`ados_clock_service::daemon::run`).

fn main() -> std::process::ExitCode {
    ados_clock_service::daemon::run()
}
