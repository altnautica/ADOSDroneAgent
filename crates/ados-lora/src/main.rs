//! `ados-lora`: the daemon body lives in the library (`ados_lora::daemon::run`).

fn main() -> std::process::ExitCode {
    ados_lora::daemon::run()
}
