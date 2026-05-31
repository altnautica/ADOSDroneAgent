//! Venv + agent package: create the Python virtualenv and install the agent
//! package into it. Required. Checkpoint `agent-package`.

use crate::ctx::Ctx;
use crate::graph::{Step, StepKind, StepOutcome};

/// Python venv creation + agent package install.
pub struct VenvAgent;

impl Step for VenvAgent {
    fn id(&self) -> &str {
        "venv_agent"
    }
    fn requires(&self) -> &[&str] {
        &["deps"]
    }
    fn checkpoint(&self) -> Option<&str> {
        Some("agent-package")
    }
    fn kind(&self) -> StepKind {
        StepKind::Required
    }
    fn run(&self, _ctx: &mut Ctx) -> StepOutcome {
        // Real venv build + pip install of the agent package lands in a later phase.
        StepOutcome::Ok
    }
}
