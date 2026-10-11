//! `ados-display`: the daemon body lives in the library (`ados_display::daemon::run_display`).

fn main() -> std::process::ExitCode {
    ados_display::daemon::run_display()
}
