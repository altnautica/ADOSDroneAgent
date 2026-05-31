//! Purge residue: clear leftovers from a prior failed/partial install (stale
//! units, half-built venvs). Optional — best-effort cleanup that never blocks
//! the install.

use crate::ctx::Ctx;
use crate::graph::{Step, StepKind, StepOutcome};

/// Best-effort cleanup of prior-install residue.
pub struct PurgeResidue;

impl Step for PurgeResidue {
    fn id(&self) -> &str {
        "purge_residue"
    }
    fn requires(&self) -> &[&str] {
        &[]
    }
    fn checkpoint(&self) -> Option<&str> {
        None
    }
    fn kind(&self) -> StepKind {
        StepKind::Optional
    }
    fn run(&self, _ctx: &mut Ctx) -> StepOutcome {
        // Real residue purge lands in a later phase.
        StepOutcome::Ok
    }
}
