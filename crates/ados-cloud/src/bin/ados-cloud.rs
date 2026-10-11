//! `ados-cloud` daemon entry point. The runtime lives in
//! [`ados_cloud::daemon`].

fn main() -> std::process::ExitCode {
    ados_cloud::daemon::run()
}
