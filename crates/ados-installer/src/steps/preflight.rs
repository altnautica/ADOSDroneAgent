//! Preflight: validate the host before any install work (arch, root, disk,
//! kernel headers presence). Required — a failed preflight aborts the install.

use crate::ctx::Ctx;
use crate::graph::{Step, StepKind, StepOutcome};

/// Host validation gate.
pub struct Preflight;

impl Step for Preflight {
    fn id(&self) -> &str {
        "preflight"
    }
    fn requires(&self) -> &[&str] {
        &[]
    }
    fn checkpoint(&self) -> Option<&str> {
        None
    }
    fn kind(&self) -> StepKind {
        StepKind::Required
    }
    fn run(&self, _ctx: &mut Ctx) -> StepOutcome {
        // Real preflight (arch / root / disk / headers) lands in a later phase.
        StepOutcome::Ok
    }
}
