//! `ados-tunnel-config`: the daemon body lives in the library (`ados_config_tunnel::daemon::run`).

fn main() -> std::process::ExitCode {
    ados_config_tunnel::daemon::run()
}
