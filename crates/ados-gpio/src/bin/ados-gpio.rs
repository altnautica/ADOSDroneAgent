//! `ados-gpio`: the daemon body lives in the library (`ados_gpio::daemon::run`).

fn main() -> std::process::ExitCode {
    ados_gpio::daemon::run()
}
