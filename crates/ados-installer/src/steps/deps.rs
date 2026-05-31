//! Deps: install the apt + python system dependencies the agent needs.
//! Required — nothing downstream works without them. Checkpoint `deps`.

use crate::ctx::Ctx;
use crate::graph::{Step, StepKind, StepOutcome};

/// System dependency installation.
pub struct Deps;

impl Step for Deps {
    fn id(&self) -> &str {
        "deps"
    }
    fn requires(&self) -> &[&str] {
        &["preflight"]
    }
    fn checkpoint(&self) -> Option<&str> {
        Some("deps")
    }
    fn kind(&self) -> StepKind {
        StepKind::Required
    }
    fn run(&self, _ctx: &mut Ctx) -> StepOutcome {
        // Real apt + pip system-dep install lands in a later phase.
        StepOutcome::Ok
    }
}
