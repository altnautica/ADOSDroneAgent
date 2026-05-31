//! systemd: render + install the service units, then `daemon-reload` and
//! `enable` them. Required. Checkpoint `systemd`. Runs only after both the
//! binaries are present and the config/identity exists.

use crate::ctx::Ctx;
use crate::graph::{Step, StepKind, StepOutcome};

/// systemd unit install + enable.
pub struct Systemd;

impl Step for Systemd {
    fn id(&self) -> &str {
        "systemd"
    }
    fn requires(&self) -> &[&str] {
        &["fetch_binaries", "config_identity"]
    }
    fn checkpoint(&self) -> Option<&str> {
        Some("systemd")
    }
    fn kind(&self) -> StepKind {
        StepKind::Required
    }
    fn run(&self, _ctx: &mut Ctx) -> StepOutcome {
        // Real unit render + daemon-reload + enable lands in a later phase.
        StepOutcome::Ok
    }
}
