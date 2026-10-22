//! `ados-lora`: the daemon body lives in the library (`ados_lora_service::daemon::run`).

fn main() -> std::process::ExitCode {
    ados_lora_service::daemon::run()
}
