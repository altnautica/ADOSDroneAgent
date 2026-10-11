//! `ados-video` entry point; the daemon body lives in [`ados_video::daemon`].

use std::process::ExitCode;

fn main() -> ExitCode {
    ados_video::daemon::run()
}
