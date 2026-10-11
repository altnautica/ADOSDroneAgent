//! `ados-net`: the daemon body lives in the library (`ados_net::daemon::run`).

fn main() -> std::process::ExitCode {
    ados_net::daemon::run()
}
