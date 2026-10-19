//! `ados-gnss`: the daemon body lives in the library (`ados_gnss::daemon::run`).

fn main() -> std::process::ExitCode {
    ados_gnss::daemon::run()
}
