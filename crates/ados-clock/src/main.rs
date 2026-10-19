//! `ados-clock`: the daemon body lives in the library (`ados_clock::daemon::run`).

fn main() -> std::process::ExitCode {
    ados_clock::daemon::run()
}
