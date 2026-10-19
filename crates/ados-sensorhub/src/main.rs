//! `ados-sensorhub`: the service body lives in the library (`ados_sensorhub::daemon::run`).

fn main() -> std::process::ExitCode {
    ados_sensorhub::daemon::run()
}
