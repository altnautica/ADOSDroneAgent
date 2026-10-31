//! Command-line surface: argument parsing + run-mode resolution.

pub mod args;
pub mod mode;

pub use args::{display_value_error, usage, Args, ParseError};
pub use mode::{render_flags, RunMode};
