//! The bodies of the crate's binaries, one module per binary.

pub mod display;
pub mod display_probe;
pub mod oled_i2c;

pub use display::run as run_display;
pub use display_probe::run as run_display_probe;
pub use oled_i2c::run as run_oled_i2c;

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
