//! Health: poll the agent REST API until it answers (the install is not "done"
//! until the agent is actually serving). Required. The final gate before the
//! install result is written.

use crate::ctx::Ctx;
use crate::graph::{Step, StepKind, StepOutcome};

/// Post-start health gate.
pub struct Health;

impl Step for Health {
    fn id(&self) -> &str {
        "health"
    }
    fn requires(&self) -> &[&str] {
        &["start"]
    }
    fn checkpoint(&self) -> Option<&str> {
        None
    }
    fn kind(&self) -> StepKind {
        StepKind::Required
    }
    fn run(&self, _ctx: &mut Ctx) -> StepOutcome {
        // Real readiness poll of the agent REST API lands in a later phase.
        StepOutcome::Ok
    }
}
