//! DKMS: build + install the RTL8812EU WFB radio kernel module. Optional —
//! matches the bash installer (a rig with no RTL adapter, or a kernel with no
//! headers, degrades rather than failing). Checkpoint `radio-driver`.

use crate::ctx::Ctx;
use crate::graph::{Step, StepKind, StepOutcome};

/// RTL8812EU DKMS build + install.
pub struct Dkms;

impl Step for Dkms {
    fn id(&self) -> &str {
        "dkms"
    }
    fn requires(&self) -> &[&str] {
        &["deps"]
    }
    fn checkpoint(&self) -> Option<&str> {
        Some("radio-driver")
    }
    fn kind(&self) -> StepKind {
        StepKind::Optional
    }
    fn run(&self, _ctx: &mut Ctx) -> StepOutcome {
        // Real DKMS build (ulimit -s unlimited, vendor source) lands in a later phase.
        StepOutcome::Ok
    }
}
