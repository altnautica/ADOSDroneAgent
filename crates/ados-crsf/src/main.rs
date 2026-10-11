//! `ados-crsf`: the daemon body lives in the library (`ados_crsf::daemon::run`).

fn main() -> std::process::ExitCode {
    ados_crsf::daemon::run()
}
