//! The bodies of the crate's binaries, one module per binary.

pub mod input;
pub mod pic;

pub use input::run as run_input;
pub use pic::run as run_pic;

/// Map a daemon body's result to the process exit code the way a
/// `fn main() -> Result<()>` reports it: `Error: {err:?}` on stderr, status 1.
fn exit_code(result: anyhow::Result<()>) -> std::process::ExitCode {
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e:?}");
            std::process::ExitCode::FAILURE
        }
    }
}
