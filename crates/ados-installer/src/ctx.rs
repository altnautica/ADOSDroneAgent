//! The mutable run context threaded through every step.
//!
//! `Ctx` carries the parsed arguments, the probed host facts, the checkpoint
//! store, and the failure accumulator the graph records into. Steps read what
//! they need and record failures here; they do not own any global state. The
//! context is cheap to build in tests via [`Ctx::for_test`].

use crate::checkpoint::Checkpoint;
use crate::cli::Args;
use crate::env::EnvInfo;
use crate::result::FailureAccumulator;

/// Per-run state shared (by `&mut`) across the step graph.
#[derive(Debug)]
pub struct Ctx {
    /// Parsed command-line arguments.
    pub args: Args,
    /// Probed host facts (arch, os).
    pub env: EnvInfo,
    /// Checkpoint store (resume markers).
    pub checkpoint: Checkpoint,
    /// Accumulated step failures; classified into the install status at the end.
    pub failures: FailureAccumulator,
    /// Whether checkpoints are bypassed this run (`--force`).
    pub force: bool,
    /// Resolved agent profile (`drone` | `ground_station`).
    pub profile: String,
    /// Release channel selector (default `edge` — clone + build from source,
    /// matching the predecessor installer's default).
    pub channel: String,
    /// The cloned source repo the install ran from. `venv_agent` records the
    /// path it cloned (edge channel) so the downstream steps (`systemd`,
    /// `config_identity`, `dkms`) can find `data/systemd`, `data/udev`, and
    /// `scripts/drivers/*`. `None` until `venv_agent` populates it; the
    /// downstream steps then fall back to `/opt/ados/source` / `INSTALL_DIR/repo`.
    pub source_dir: Option<std::path::PathBuf>,
}

impl Ctx {
    /// Build the run context from parsed arguments. The profile defaults to
    /// `drone` and the channel to `edge` when the flags are absent (edge =
    /// clone + build from source, the predecessor installer's default).
    pub fn from_args(args: Args, env: EnvInfo, checkpoint: Checkpoint) -> Self {
        let force = args.force;
        let profile = args.profile.clone().unwrap_or_else(|| "drone".to_string());
        let channel = args.channel.clone().unwrap_or_else(|| "edge".to_string());
        Ctx {
            args,
            env,
            checkpoint,
            failures: FailureAccumulator::new(),
            force,
            profile,
            channel,
            source_dir: None,
        }
    }

    /// A minimal context for unit tests: drone profile, given checkpoint root,
    /// probed env, default args, force off.
    pub fn for_test(checkpoint: Checkpoint) -> Self {
        Ctx::from_args(Args::default(), EnvInfo::probe(), checkpoint)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Args;

    #[test]
    fn from_args_defaults_profile_and_channel() {
        let ctx = Ctx::from_args(Args::default(), EnvInfo::probe(), Checkpoint::new());
        assert_eq!(ctx.profile, "drone");
        assert_eq!(ctx.channel, "edge");
        assert!(!ctx.force);
    }

    #[test]
    fn from_args_carries_profile_force_channel() {
        let a = Args {
            profile: Some("ground_station".to_string()),
            channel: Some("edge".to_string()),
            force: true,
            ..Args::default()
        };
        let ctx = Ctx::from_args(a, EnvInfo::probe(), Checkpoint::new());
        assert_eq!(ctx.profile, "ground_station");
        assert_eq!(ctx.channel, "edge");
        assert!(ctx.force);
    }
}
