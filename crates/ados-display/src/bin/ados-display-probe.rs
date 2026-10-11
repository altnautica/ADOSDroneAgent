//! `ados-display-probe`: the daemon body lives in the library (`ados_display::daemon::run_display_probe`).

fn main() -> std::process::ExitCode {
    ados_display::daemon::run_display_probe()
}
