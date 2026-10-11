//! `ados-oled-i2c`: the daemon body lives in the library (`ados_display::daemon::run_oled_i2c`).

fn main() -> std::process::ExitCode {
    ados_display::daemon::run_oled_i2c()
}
