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
    /// Whether the `extensions` step installs the first-party World Engine
    /// extension: the `--world-engine` / `--no-world-engine` choice (the wizard
    /// writes the same field), else the profile default
    /// ([`crate::steps::extensions::world_engine_default`]).
    pub install_world_engine: bool,
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
    /// The revision every artifact is pinned to (`--ref`), or `None` for the
    /// normal rolling install.
    ///
    /// `venv_agent` overwrites an abbreviated value with the full 40-character
    /// object name as soon as the clone can resolve it, and it runs before
    /// `fetch_binaries`, so the binary fetch always builds `rev-<full sha>` —
    /// the tag CI publishes — without asking GitHub to expand a prefix. Steps
    /// read this, never `args.rev`, so nothing downstream can see the
    /// unexpanded form.
    pub rev: Option<String>,
    /// The directory of locally-built service binaries this install places
    /// instead of fetching (`--artifacts`), or `None` for the normal
    /// fetch-from-release install.
    ///
    /// Read by `fetch_binaries`, which resolves each catalog entry to a local
    /// file when the directory carries one and to the release otherwise. The
    /// bytes then take the SAME verify → chmod → atomic-replace → `.prev`
    /// retention path either way.
    pub artifacts: Option<std::path::PathBuf>,
    /// Live-progress sink. Defaults to a no-op; the binary swaps in a real sink
    /// after starting the renderer. Steps and the graph emit progress through it.
    pub progress: ProgressSink,
    /// Provisioning that is staged but needs a reboot to take effect, one
    /// reason per entry, as recorded in `/run/ados/reboot-required` by the
    /// overlay/dtparam provisioners and read back by the `reboot` step.
    ///
    /// Carried on the context rather than acted on in the step because the
    /// reboot has to happen AFTER the closing summary is drawn — a step that
    /// rebooted mid-graph would kill the renderer and the operator would never
    /// learn why the box went away.
    pub pending_reboot: Vec<String>,
}

/// Resolve the release channel for this run: the `--channel` flag, else the
/// persisted `/etc/ados/profile.conf` value, else `edge`.
///
/// Shared with the pre-install validation in the binary, which has to know the
/// channel BEFORE a `Ctx` exists (a `--ref` pin is refused on `stable`, and the
/// box's persisted channel counts just as much as the flag).
///
/// The persisted fallback exists because an upgrade with no `--channel` used to
/// fall back to the compiled-in `edge` default, so a device deliberately
/// installed on `stable` defected to tip-of-main on its first update. Nobody
/// chose that; it is one keystroke from the status screen.
///
/// The default stays `edge`, and NOT because signature verification is
/// acceptable to leave off. The channel decides two unrelated things: how
/// strict verification is, and where the agent package comes from. On `stable`
/// the second meaning is "install this pinned release wheel", and `venv_agent`
/// fails outright with "stable channel requires --version" when nothing is
/// pinned. There is no resolve-the-latest-release path. A fresh box has no
/// persisted version, so making `stable` the default would abort every flag-less
/// install at the provision step — the verification posture would be irrelevant
/// because nothing would finish installing. Publishing a resolvable latest
/// release is what unblocks that default, and it is a release-process change,
/// not a resolution change.
///
/// So the verification hole is closed where it actually was, at the fetch: a
/// signature that does not match the vendored trust anchor is now refused on
/// every channel, this one included. That also answers the box already carrying
/// `channel: edge` in its `profile.conf` — and both rigs do. It gains the check
/// on its next upgrade with no re-pin, whereas moving the default would have
/// helped only boxes installed after it moved. What remains channel-gated is
/// the weaker tolerance of a signature that cannot be OBTAINED. That tolerance
/// is a fallback for an artifact published before signing existed, not the
/// normal case: every current release carries a signature, and the vendored
/// anchor validates them. `preflight::lenient_channel_note` reports the
/// tolerance rather than asserting it is in use, because a warning that fires
/// when nothing is wrong is one an operator learns to ignore.
pub fn resolve_channel(flag: Option<&str>) -> String {
    flag.map(str::to_string)
        .or_else(crate::env::read_persisted_channel)
        .unwrap_or_else(|| crate::verify::EDGE_CHANNEL.to_string())
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
        let channel = resolve_channel(args.channel.as_deref());
        let install_rtl8812eu = !args.no_rtl_driver;
        let install_world_engine = args
            .world_engine
            .unwrap_or_else(|| crate::steps::extensions::world_engine_default(&profile));
        // A pinned channel installs an explicit release, so an upgrade with no
        // `--version` must reuse the pinned one rather than fail or drift.
        let mut args = args;
        if args.version.is_none() {
            args.version = crate::env::read_persisted_version();
        }
        let rev = args.rev.clone();
        let artifacts = args.artifacts.as_deref().map(std::path::PathBuf::from);
        Ctx {
            args,
            env,
            checkpoint,
            failures: FailureAccumulator::new(),
            force,
            profile,
            install_rtl8812eu,
            install_world_engine,
            channel,
            region_pinned: None,
            cloud_from_anywhere: false,
            source_dir: None,
            rev,
            artifacts,
            progress: ProgressSink::default(),
            pending_reboot: Vec::new(),
        }
    }

    /// A minimal context for unit tests: drone profile, given checkpoint root,
    /// probed env, default args, force off.
    pub fn for_test(checkpoint: Checkpoint) -> Self {
        Ctx::from_args(Args::default(), EnvInfo::probe(), checkpoint)
    }
}

/// Why a `--ref` pin cannot be honoured on `channel` (pure). `None` when the
/// combination is installable.
///
/// `--ref` names a git commit, and only the `edge` channel has anything
/// commit-addressable to point at: it clones the tree, and CI publishes that
/// commit's binaries under `rev-<sha>`. The `stable` channel resolves its wheel
/// from a `v<X.Y.Z>` release tag, which is a version and not a revision — there
/// is no commit to select and no per-revision wheel to install. Accepting the
/// pair and honouring only the binary half would report a pin the wheel does not
/// have, which is the exact split (`wheel from one revision, binaries from
/// another`) that `--ref` exists to close, so it is refused instead.
///
/// The channel argument is the RESOLVED one, not the flag: a box whose
/// `profile.conf` already says `stable` reaches this with no `--channel` on the
/// command line at all.
pub fn rev_channel_conflict(rev: Option<&str>, channel: &str) -> Option<String> {
    let rev = rev?;
    if channel != "stable" {
        return None;
    }
    Some(format!(
        "--ref {rev} cannot be honoured on the stable channel: stable installs a \
         release wheel resolved from a v<X.Y.Z> tag, which addresses a version \
         rather than a commit, so there is no per-revision wheel to install. \
         Re-run with `--channel edge --ref {rev}` to pin the tree and the \
         prebuilt binaries to that commit, or drop --ref to install the pinned \
         release."
    ))
}

/// Why a `--artifacts <dir>` install cannot be honoured alongside `channel` /
/// `rev` (pure). `None` when the combination is installable.
///
/// Two refusals, both because the alternative is a check that silently does not
/// apply:
///
/// * **`--channel stable`.** A locally-built binary carries no `.minisig`: the
///   signing key is a CI secret, so nothing off the release job can produce one.
///   `stable` refuses an artifact whose signature cannot be obtained, so the
///   pair would abort partway through the binary loop with a per-binary
///   signature message that reads as a broken release rather than as an
///   impossible request. Said here, before any work, it names the real cause.
/// * **`--ref`.** The pin exists to guarantee the agent package and every
///   service binary come from ONE commit. A local artifact directory is by
///   definition not that commit — honouring both would produce exactly the
///   wheel-from-one-revision-binary-from-another split `--ref` was added to
///   close, while still reporting the pin as applied.
pub fn artifacts_conflict(
    artifacts: Option<&str>,
    rev: Option<&str>,
    channel: &str,
) -> Option<String> {
    let dir = artifacts?;
    if channel == "stable" {
        return Some(format!(
            "--artifacts {dir} cannot be honoured on the stable channel: the \
             stable channel refuses an artifact whose signature it cannot \
             verify, and a locally-built binary has none (the signing key is a \
             CI secret). Re-run with `--channel edge --artifacts {dir}`, where a \
             missing signature is a warning and the SHA256 sidecar is still \
             mandatory."
        ));
    }
    if let Some(rev) = rev {
        return Some(format!(
            "--artifacts {dir} and --ref {rev} both decide where the service \
             binaries come from. --ref exists to guarantee the agent package \
             and every binary come from one commit, which a local build is not, \
             so honouring both would report a pin the binaries do not have. Use \
             one: --ref {rev} to install that commit's published binaries, or \
             --artifacts {dir} to install your build."
        ));
    }
    None
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

    #[test]
    fn the_world_engine_follows_the_profile_unless_a_flag_pins_it() {
        let ctx_for = |profile: &str, choice: Option<bool>| {
            let a = Args {
                profile: Some(profile.to_string()),
                world_engine: choice,
                ..Args::default()
            };
            Ctx::from_args(a, EnvInfo::probe(), Checkpoint::new()).install_world_engine
        };
        // Workstation-class nodes are where the engine runs its heavy half.
        assert!(ctx_for("workstation", None));
        assert!(ctx_for("compute", None));
        // An aircraft or a ground station opts in explicitly.
        assert!(!ctx_for("drone", None));
        assert!(!ctx_for("ground_station", None));
        // An explicit flag wins over the profile default in both directions.
        assert!(!ctx_for("workstation", Some(false)));
        assert!(ctx_for("drone", Some(true)));
    }

    #[test]
    fn from_args_carries_the_revision_pin_into_the_context() {
        let a = Args {
            rev: Some("3b4b8dee".to_string()),
            ..Args::default()
        };
        let ctx = Ctx::from_args(a, EnvInfo::probe(), Checkpoint::new());
        assert_eq!(ctx.rev.as_deref(), Some("3b4b8dee"));
        // Absent by default: an unpinned install must behave exactly as before.
        let plain = Ctx::from_args(Args::default(), EnvInfo::probe(), Checkpoint::new());
        assert!(plain.rev.is_none());
    }

    #[test]
    fn a_revision_pin_is_refused_on_the_stable_channel_and_allowed_on_edge() {
        let refusal = rev_channel_conflict(Some("3b4b8dee"), "stable")
            .expect("stable has no commit addressing, so the pair must be refused");
        // The message has to say WHY, or the operator retries the same thing.
        assert!(
            refusal.contains("v<X.Y.Z>"),
            "names what stable resolves: {refusal}"
        );
        assert!(
            refusal.contains("--channel edge"),
            "names the fix: {refusal}"
        );

        assert!(rev_channel_conflict(Some("3b4b8dee"), "edge").is_none());
        // No pin, no conflict — on any channel.
        assert!(rev_channel_conflict(None, "stable").is_none());
        assert!(rev_channel_conflict(None, "edge").is_none());
    }

    #[test]
    fn an_artifacts_directory_reaches_the_context_and_defaults_absent() {
        let a = Args {
            artifacts: Some("/srv/build/release".to_string()),
            ..Args::default()
        };
        let ctx = Ctx::from_args(a, EnvInfo::probe(), Checkpoint::new());
        assert_eq!(
            ctx.artifacts.as_deref(),
            Some(std::path::Path::new("/srv/build/release"))
        );
        let plain = Ctx::from_args(Args::default(), EnvInfo::probe(), Checkpoint::new());
        assert!(
            plain.artifacts.is_none(),
            "an install with no flag must fetch exactly as before"
        );
    }

    #[test]
    fn artifacts_are_refused_on_stable_and_alongside_a_revision_pin() {
        // Both refusals exist because the alternative is a check that silently
        // does not apply, so both messages have to name the real cause: on
        // stable, the signature a local build cannot have; with --ref, the pin it
        // would contradict.
        let stable = artifacts_conflict(Some("/srv/build"), None, "stable")
            .expect("stable cannot verify an unsigned local build");
        assert!(stable.contains("signature"), "names the cause: {stable}");
        assert!(stable.contains("--channel edge"), "names the fix: {stable}");

        let pinned = artifacts_conflict(Some("/srv/build"), Some("3b4b8dee"), "edge")
            .expect("--ref and --artifacts both decide where binaries come from");
        assert!(pinned.contains("3b4b8dee"), "names the pin: {pinned}");

        // The supported combination, and the no-flag case on every channel.
        assert!(artifacts_conflict(Some("/srv/build"), None, "edge").is_none());
        assert!(artifacts_conflict(None, Some("3b4b8dee"), "edge").is_none());
        assert!(artifacts_conflict(None, None, "stable").is_none());
    }
}
