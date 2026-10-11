//! `ados-camera-probe`: the daemon body lives in the library (`ados_hal_probe::daemon::run`).

fn main() -> std::process::ExitCode {
    ados_hal_probe::daemon::run()
}
