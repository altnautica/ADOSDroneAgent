//! Child-process primitives shared by the agent daemons.
//!
//! - [`group`]: a child spawned as its own process-group leader, terminated or
//!   killed as a whole group, with a `Drop` backstop.
//! - [`lines`]: a line reader that bounds each line and splits on `\n` or `\r`.
//! - [`logcap`]: empty a child's log file once it outgrows a cap.
//! - [`run`]: run a short command with a timeout and capture its output.

pub mod group;
pub mod lines;
pub mod logcap;
pub mod run;

pub use group::{GroupChild, Spec, StderrMode, StdoutMode};
