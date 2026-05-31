//! Start: start the supervisor unit (which brings up the profile's service
//! set). Required. Runs only after the units are installed and the binaries
//! they exec are present.

use crate::ctx::Ctx;
use crate::graph::{Step, StepKind, StepOutcome};

/// Start the top-level supervisor unit.
pub struct Start;

impl Step for Start {
    fn id(&self) -> &str {
        "start"
    }
    fn requires(&self) -> &[&str] {
        &["systemd", "fetch_binaries"]
    }
    fn checkpoint(&self) -> Option<&str> {
        None
    }
    fn kind(&self) -> StepKind {
        StepKind::Required
    }
    fn run(&self, _ctx: &mut Ctx) -> StepOutcome {
        // Real `systemctl start ados-supervisor` lands in a later phase.
        StepOutcome::Ok
    }
}
