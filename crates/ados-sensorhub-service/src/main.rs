//! `ados-sensorhub`: the service body lives in the library (`ados_sensorhub_service::daemon::run`).

fn main() -> std::process::ExitCode {
    ados_sensorhub_service::daemon::run()
}
