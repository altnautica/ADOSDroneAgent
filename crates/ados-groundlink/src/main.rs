//! `ados-groundlink` binary — the ground-station data-plane service. The service
//! lives in [`ados_groundlink::daemon`]; this entry only hands over to it.

fn main() -> std::process::ExitCode {
    ados_groundlink::daemon::run()
}
