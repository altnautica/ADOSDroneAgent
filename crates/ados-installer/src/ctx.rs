//! The mutable run context threaded through every step.
//!
//! `Ctx` carries the parsed arguments, the probed host facts, the checkpoint
//! store, and the failure accumulator the graph records into. Steps read what
//! they need and record failures here; they do not own any global state. The
//! context is cheap to build in tests via [`Ctx::for_test`].

use crate::checkpoint::Checkpoint;
use crate::cli::args::normalize_profile;
use crate::cli::Args;
use crate::env::EnvInfo;
use crate::result::FailureAccumulator;
use crate::ui::ProgressSink;

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
    /// Resolved agent profile (`drone` | `ground_station` | `workstation` | `compute`).
    pub profile: String,
    /// Whether to build + install the RTL8812EU WFB radio driver. Default on;
    /// `--no-rtl-driver` opts out (a workstation/compute node or a rig with no
    /// long-range radio does not need it). The `dkms` step honours this.
    pub install_rtl8812eu: bool,
    /// Release channel selector (default `edge` — clone + build from source,
    /// matching the predecessor installer's default).
    pub channel: String,
    /// Pinned operating region (ISO 3166-1 alpha-2), or `None` for the default
    /// unrestricted radio posture. Set by the onboarding wizard; the config step
    /// writes the matching `network.regulatory` block.
    pub region_pinned: Option<String>,
    /// The operator asked to reach this device from anywhere (cloud relay on).
    /// Default `false` keeps it local-first; the config step writes `server.mode`
    /// accordingly.
    pub cloud_from_anywhere: bool,
    /// The cloned source repo the install ran from. `venv_agent` records the
    /// path it cloned (edge channel) so the downstream steps (`systemd`,
    /// `config_identity`, `dkms`) can find `data/systemd`, `data/udev`, and
    /// `scripts/drivers/*`. `None` until `venv_agent` populates it; the
    /// downstream steps then fall back to `/opt/ados/source` / `INSTALL_DIR/repo`.
    pub source_dir: Option<std::path::PathBuf>,
    /// Live-progress sink. Defaults to a no-op; the binary swaps in a real sink
    /// after starting the renderer. Steps and the graph emit progress through it.
    pub progress: ProgressSink,
}

impl Ctx {
    /// Build the run context from parsed arguments. The profile defaults to
    /// `drone` and the channel to `edge` when the flags are absent (edge =
    /// clone + build from source, the predecessor installer's default).
    pub fn from_args(args: Args, env: EnvInfo, checkpoint: Checkpoint) -> Self {
        let force = args.force;
        // Resolve the profile: an explicit `--profile` wins; otherwise fall back
        // to the persisted `/etc/ados/profile.conf` so an `--upgrade` /
        // `ados update` with no flag PRESERVES a non-drone box's profile instead
        // of re-provisioning it as the `drone` default and tearing down its
        // profile units. A fresh box has no such file, so it keeps `drone`.
        // (`crate::env::` is fully qualified: the `env` parameter shadows the
        // module in this scope.)
        let profile = args
            .profile
            .clone()
            .or_else(crate::env::read_persisted_profile)
            .map(|p| normalize_profile(&p))
            .unwrap_or_else(|| "drone".to_string());
        // Same preservation rule as the profile, and for a sharper reason: an
        // upgrade with no `--channel` used to fall back to the compiled-in
        // `edge` default, so a device deliberately installed on `stable`
        // defected to tip-of-main on its first update. Nobody chose that; it is
        // one keystroke from the status screen.
        //
        // The default stays `edge`, and NOT because signature verification is
        // acceptable to leave off. The channel decides two unrelated things: how
        // strict verification is, and where the agent package comes from. On
        // `stable` the second meaning is "install this pinned release wheel",
        // and `venv_agent` fails outright with "stable channel requires
        // --version" when nothing is pinned. There is no resolve-the-latest-
        // release path. A fresh box has no persisted version, so making `stable`
        // the default would abort every flag-less install at the provision step
        // — the verification posture would be irrelevant because nothing would
        // finish installing. Publishing a resolvable latest release is what
        // unblocks that default, and it is a release-process change, not a
        // resolution change.
        //
        // So the verification hole is closed where it actually was, at the
        // fetch: a signature that does not match the vendored trust anchor is
        // now refused on every channel, this one included. That also answers the
        // box already carrying `channel: edge` in its `profile.conf` — and both
        // rigs do. It gains the check on its next upgrade with no re-pin,
        // whereas moving the default would have helped only boxes installed
        // after it moved. What remains channel-gated is the weaker tolerance of
        // a signature that cannot be OBTAINED. That tolerance is a fallback for
        // an artifact published before signing existed, not the normal case:
        // every current release carries a signature, and the vendored anchor
        // validates them. `preflight::lenient_channel_note` reports the
        // tolerance rather than asserting it is in use, because a warning that
        // fires when nothing is wrong is one an operator learns to ignore.
        let channel = args
            .channel
            .clone()
            .or_else(crate::env::read_persisted_channel)
            .unwrap_or_else(|| crate::verify::EDGE_CHANNEL.to_string());
        let install_rtl8812eu = !args.no_rtl_driver;
        // A pinned channel installs an explicit release, so an upgrade with no
        // `--version` must reuse the pinned one rather than fail or drift.
        let mut args = args;
        if args.version.is_none() {
            args.version = crate::env::read_persisted_version();
        }
        Ctx {
            args,
            env,
            checkpoint,
            failures: FailureAccumulator::new(),
            force,
            profile,
            install_rtl8812eu,
            channel,
            region_pinned: None,
            cloud_from_anywhere: false,
            source_dir: None,
            progress: ProgressSink::default(),
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
    fn the_default_channel_is_one_a_flagless_install_can_actually_finish() {
        // `stable` is not just a stricter verification posture, it also selects
        // where the agent package comes from: `venv_agent::install_agent_stable`
        // bails with "stable channel requires --version" when no version is
        // pinned, and there is no resolve-the-latest-release path. A flag-less
        // install on a fresh box has no persisted version, so a `stable` default
        // would abort every one of them at the provision step.
        //
        // This is a standing constraint, not a preference. Anyone moving the
        // default to `stable` has to give the installer a way to resolve a
        // version first, and this fails until they do rather than letting the
        // breakage be discovered on a rig.
        let ctx = Ctx::from_args(Args::default(), EnvInfo::probe(), Checkpoint::new());
        assert!(
            ctx.channel != "stable" || ctx.args.version.is_some(),
            "a default install on the stable channel needs a version to install; \
             see venv_agent::install_agent_stable"
        );
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
