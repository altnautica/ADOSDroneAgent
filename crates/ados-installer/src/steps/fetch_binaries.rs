//! Fetch binaries: download + verify the prebuilt Rust service binaries for
//! the active profile and install them under `/opt/ados/bin`, then install the
//! global `ados*` symlinks. Required. Checkpoint `global-symlinks`.

use crate::ctx::Ctx;
use crate::graph::{Step, StepKind, StepOutcome};

/// Prebuilt-binary fetch + global symlink install.
pub struct FetchBinaries;

impl Step for FetchBinaries {
    fn id(&self) -> &str {
        "fetch_binaries"
    }
    fn requires(&self) -> &[&str] {
        &["deps"]
    }
    fn checkpoint(&self) -> Option<&str> {
        Some("global-symlinks")
    }
    fn kind(&self) -> StepKind {
        StepKind::Required
    }
    fn run(&self, _ctx: &mut Ctx) -> StepOutcome {
        // Real prebuilt fetch (curl-shell-out + sha + Ed25519 verify) + symlink
        // install lands in a later phase. The catalog is in `crate::binaries`.
        StepOutcome::Ok
    }
}
