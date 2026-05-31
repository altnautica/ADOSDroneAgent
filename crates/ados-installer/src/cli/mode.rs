//! Resolve the install run-mode from parsed arguments.
//!
//! The mode picks which step chain the installer assembles. In this scaffold
//! the resolution is purely flag-driven; the "already installed" probe that
//! turns a bare pair code into a `PairOnly` (instead of a `FreshInstall`) lands
//! in a later phase, so `PairOnly` is only reached via an explicit caller hook
//! for now.

use super::args::Args;

/// What the installer is being asked to do this run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    /// First-time install: run the full step chain.
    FreshInstall,
    /// Upgrade an existing install in place.
    Upgrade,
    /// `--force`: clear checkpoints and reinstall from scratch.
    ForceReinstall,
    /// Re-pair only against an already-installed agent (no install work).
    PairOnly,
    /// Remove the agent.
    Uninstall,
    /// Print install status and exit.
    Status,
}

impl RunMode {
    /// Derive the mode from flags. Precedence: explicit actions
    /// (status, uninstall) first, then force, then upgrade, else a fresh
    /// install.
    ///
    /// `already_installed` is the result of the (later-phase) install probe.
    /// A bare pair code against an existing install with no `--force` is a
    /// `PairOnly`; in this phase the caller passes `false` so the flag-only
    /// path is exercised, and `PairOnly` is opt-in via this argument.
    pub fn resolve(args: &Args, already_installed: bool) -> RunMode {
        if args.status {
            return RunMode::Status;
        }
        if args.uninstall {
            return RunMode::Uninstall;
        }
        if args.force {
            return RunMode::ForceReinstall;
        }
        // A pair-only run: an existing install, a pair code, and no reinstall.
        if already_installed && args.pair.is_some() && !args.upgrade {
            return RunMode::PairOnly;
        }
        if args.upgrade {
            return RunMode::Upgrade;
        }
        RunMode::FreshInstall
    }

    /// True when the mode runs the install step chain (vs a pure action).
    pub fn runs_install_chain(self) -> bool {
        matches!(
            self,
            RunMode::FreshInstall | RunMode::Upgrade | RunMode::ForceReinstall
        )
    }

    /// True when checkpoints must be cleared before the run.
    ///
    /// Both a force reinstall and an upgrade clear them. An upgrade MUST: the
    /// per-step checkpoints (`venv`, `systemd`, `global-symlinks`,
    /// `radio-driver`, `deps`) exist to resume an interrupted FRESH install, so
    /// if they survive into an upgrade the graph would skip the very steps that
    /// refetch the new code, units, and prebuilt binaries — `--upgrade` would
    /// silently do nothing. Clearing them makes the upgrade re-clone + reinstall
    /// the agent package, redeploy the units, re-fetch the binaries, and top up
    /// system packages (a new version may need new deps). The venv itself is
    /// preserved on upgrade (only `--force` rebuilds it).
    pub fn clears_checkpoints(self) -> bool {
        matches!(self, RunMode::ForceReinstall | RunMode::Upgrade)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_fresh_install() {
        assert_eq!(
            RunMode::resolve(&Args::default(), false),
            RunMode::FreshInstall
        );
    }

    #[test]
    fn status_wins_over_everything() {
        let a = Args {
            status: true,
            force: true,
            upgrade: true,
            ..Args::default()
        };
        assert_eq!(RunMode::resolve(&a, true), RunMode::Status);
    }

    #[test]
    fn uninstall_wins_over_install_flags() {
        let a = Args {
            uninstall: true,
            upgrade: true,
            ..Args::default()
        };
        assert_eq!(RunMode::resolve(&a, true), RunMode::Uninstall);
    }

    #[test]
    fn force_implies_force_reinstall() {
        let a = Args {
            force: true,
            ..Args::default()
        };
        let m = RunMode::resolve(&a, true);
        assert_eq!(m, RunMode::ForceReinstall);
        assert!(m.clears_checkpoints());
        assert!(m.runs_install_chain());
    }

    #[test]
    fn upgrade_flag_is_upgrade() {
        let a = Args {
            upgrade: true,
            ..Args::default()
        };
        let m = RunMode::resolve(&a, true);
        assert_eq!(m, RunMode::Upgrade);
        // An upgrade must clear checkpoints so the agent/units/binaries
        // actually refetch — otherwise the resume checkpoints make it a no-op.
        assert!(m.clears_checkpoints());
        assert!(m.runs_install_chain());
    }

    #[test]
    fn pair_code_on_existing_install_is_pair_only() {
        let a = Args {
            pair: Some("CODE".to_string()),
            ..Args::default()
        };
        assert_eq!(RunMode::resolve(&a, true), RunMode::PairOnly);
        // Same flags but not yet installed → a fresh install (pairs at the end).
        assert_eq!(RunMode::resolve(&a, false), RunMode::FreshInstall);
        // With --force the pair code does not short-circuit to PairOnly.
        let forced = Args { force: true, ..a };
        assert_eq!(RunMode::resolve(&forced, true), RunMode::ForceReinstall);
    }
}
