//! The interactive onboarding wizard.
//!
//! On a fresh, interactive install the wizard walks the operator through the
//! few choices the agent needs (profile, components, Wi-Fi, name, pairing),
//! collecting them into [`Args`] plus a small [`WizardExtras`] for the install
//! to act on exactly as the flag-driven path would. It renders over a
//! self-opened `/dev/tty` (see [`crate::ui::tty`]) so it works under
//! `curl … | sudo bash`, where stdin is the piped script rather than the
//! keyboard.
//!
//! The wizard NEVER runs when a decisive flag already answered the questions,
//! in a machine/CI context, or when no controlling terminal is available. In
//! those cases the install proceeds silently with the auto-detected defaults —
//! a fresh box still comes up fully installed with zero follow-up commands.

pub mod frame;
pub mod render;
pub mod widgets;

mod catalog;
mod hw;
mod screens;
mod wifi;

use crate::cli::Args;
use crate::ui::theme::Theme;
use crate::ui::tty::Tty;

/// Choices the wizard collects that are not existing [`Args`] fields. They are
/// applied to the run context after parsing, so the install writes them at
/// config time exactly as the removed post-boot setup step used to.
#[derive(Debug, Clone, Default)]
pub struct WizardExtras {
    /// Pinned operating region (ISO 3166-1 alpha-2), or `None` for the default
    /// unrestricted posture.
    pub region_pinned: Option<String>,
    /// The operator asked to reach the device from anywhere (cloud relay on).
    /// Default `false` keeps the device local-first.
    pub cloud_from_anywhere: bool,
}

/// The result of a wizard attempt.
pub enum WizardControl {
    /// The operator finished; apply `WizardExtras` and continue the install.
    Completed(WizardExtras),
    /// No controlling terminal — proceed with the silent, flag-driven path.
    Skipped,
    /// The operator canceled (Ctrl-C); the caller stops cleanly.
    Canceled,
}

/// Whether the interactive wizard should run for this invocation.
///
/// Interactive only when the operator did NOT pin the answers with a decisive
/// flag, we are not in a machine/CI mode, and a controlling terminal is
/// actually reachable. Anything else falls to the silent auto-detected install.
pub fn should_run(args: &Args) -> bool {
    flags_allow_wizard(args) && env_allows_interactive() && Tty::is_available()
}

/// The pure part of the gate: do the parsed flags leave room for the wizard?
/// (No environment, no terminal probe — unit-tested in isolation.)
///
/// Any decisive flag means an automation caller already answered:
/// `--non-interactive` / `--yes` force the silent path; `--json` / `--quiet`
/// are machine modes; a pinned `--profile` / `--pair` or an `--upgrade` is an
/// answered, non-first-run install.
pub fn flags_allow_wizard(args: &Args) -> bool {
    !args.non_interactive
        && !args.yes
        && !args.json
        && !args.quiet
        && args.profile.is_none()
        && args.pair.is_none()
        && !args.upgrade
}

/// True when the environment permits interactive UI: not CI, and `TERM` is not
/// the no-capability `dumb` terminal.
fn env_allows_interactive() -> bool {
    std::env::var_os("CI").is_none() && std::env::var("TERM").map(|t| t != "dumb").unwrap_or(true)
}

/// Run the wizard, collecting the operator's choices into `args` and returning
/// the extra (region / cloud) choices. Opens `/dev/tty`; returns
/// [`WizardControl::Skipped`] when it cannot (the caller then installs silently).
///
/// On [`WizardControl::Completed`] the still-open `Tty` is handed back so the
/// install progress renders in the SAME alternate-screen session (one seamless
/// full-screen flow from onboarding into the install). On `Skipped` / `Canceled`
/// the `Tty` is dropped here, leaving the alt screen so a cancel message prints
/// on the normal terminal.
pub fn run(args: &mut Args) -> anyhow::Result<(WizardControl, Option<Tty>)> {
    let theme = Theme::detect(args.no_color, args.ascii);
    let mut tty = match Tty::open()? {
        Some(t) => t,
        None => return Ok((WizardControl::Skipped, None)),
    };

    let mut hw = hw::probe();
    let mut extras = WizardExtras::default();
    let mut collected = screens::Collected::default();

    if !screens::greet(&mut tty, &theme) {
        return Ok((WizardControl::Canceled, None));
    }
    match screens::run_stages(&mut tty, &theme, args, &mut hw, &mut extras, &mut collected) {
        screens::Outcome::Completed => Ok((WizardControl::Completed(extras), Some(tty))),
        screens::Outcome::Canceled => Ok((WizardControl::Canceled, None)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Args;

    #[test]
    fn wizard_gate_open_on_a_bare_fresh_invocation() {
        // No flags at all → the flags permit the wizard (the tty + env checks
        // are separate).
        assert!(flags_allow_wizard(&Args::default()));
    }

    #[test]
    fn decisive_flags_close_the_gate() {
        let cases = [
            Args {
                non_interactive: true,
                ..Args::default()
            },
            Args {
                yes: true,
                ..Args::default()
            },
            Args {
                json: true,
                ..Args::default()
            },
            Args {
                quiet: true,
                ..Args::default()
            },
            Args {
                profile: Some("drone".into()),
                ..Args::default()
            },
            Args {
                pair: Some("ABCD-1234".into()),
                ..Args::default()
            },
            Args {
                upgrade: true,
                ..Args::default()
            },
        ];
        for a in cases {
            assert!(
                !flags_allow_wizard(&a),
                "a decisive flag should close the wizard gate: {a:?}"
            );
        }
    }
}
