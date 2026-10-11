//! `ados-input`: the daemon body lives in the library (`ados_hid::daemon::run_input`).

fn main() -> std::process::ExitCode {
    ados_hid::daemon::run_input()
}
