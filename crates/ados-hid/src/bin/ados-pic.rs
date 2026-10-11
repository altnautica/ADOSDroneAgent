//! `ados-pic`: the daemon body lives in the library (`ados_hid::daemon::run_pic`).

fn main() -> std::process::ExitCode {
    ados_hid::daemon::run_pic()
}
