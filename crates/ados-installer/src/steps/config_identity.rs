//! Config + identity: write `/etc/ados/config.yaml` + `profile.conf`, mint the
//! device id, and persist the pairing material. Required — the agent will not
//! resolve its profile or identity without it.

use crate::ctx::Ctx;
use crate::graph::{Step, StepKind, StepOutcome};

/// Operator config + agent identity provisioning.
pub struct ConfigIdentity;

impl Step for ConfigIdentity {
    fn id(&self) -> &str {
        "config_identity"
    }
    fn requires(&self) -> &[&str] {
        &["venv_agent"]
    }
    fn checkpoint(&self) -> Option<&str> {
        None
    }
    fn kind(&self) -> StepKind {
        StepKind::Required
    }
    fn run(&self, _ctx: &mut Ctx) -> StepOutcome {
        // Real config write + device-id mint + pairing persist lands in a later phase.
        StepOutcome::Ok
    }
}
