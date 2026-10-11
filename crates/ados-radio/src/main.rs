//! `ados-radio` binary — the WFB TX service for the drone profile. The service
//! lives in [`ados_radio::daemon`]; this entry only hands over to it.

fn main() -> std::process::ExitCode {
    ados_radio::daemon::run()
}
