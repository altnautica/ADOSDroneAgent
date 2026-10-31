//! The wizard's step spine: one screen per decision, with Back support.
//!
//! Each stage shows the auto-detected default, takes one primary action, and
//! writes its result into `Args` / `WizardExtras`. The stages are walked in
//! order with Esc going back a step; the review screen finishes or re-enters an
//! earlier answer. Every screen reads through [`ados_term::tty::Tty`] and
//! renders through [`ados_term::theme`], so it degrades cleanly to no-color and
//! ASCII terminals.

use ados_term::theme::Theme;
use ados_term::tty::Tty;
use ados_term::widgets::{select_list, Choice, Flow};

use crate::cli::Args;
use crate::wizard::{hw, widgets, WizardExtras};

mod identity;
mod review;
mod role;
mod wifi_flow;

use identity::{name_stage, pair_stage};
use review::review_stage;
use role::{components_stage, hardware_stage, profile_stage};
use wifi_flow::wifi_stage;

/// Cross-stage collected state that is not an `Args` field (the joined Wi-Fi
/// SSID, for the review summary).
#[derive(Debug, Default)]
pub struct Collected {
    pub wifi_ssid: Option<String>,
}

/// The terminal result of the whole spine.
pub enum Outcome {
    Completed,
    Canceled,
}

/// One stage's navigation result.
enum Nav {
    Next,
    Back,
    Abort,
}

/// The card section label shown in the title bar for the general steps.
const SETUP: &str = "setup";

/// The card section label for the Wi-Fi sub-flow.
const WIFI: &str = "Wi-Fi";

/// The one-time welcome screen. Returns `false` if the operator cancels here.
pub fn greet(tty: &mut Tty, theme: &Theme) -> bool {
    tty.set_chrome(0, 0, "");
    let intro = vec![
        theme.heading("Welcome to ADOS"),
        String::new(),
        theme.dim("Let's set up this device. It takes about 3 minutes."),
        theme.dim("Use the arrow keys to move and Enter to choose."),
    ];
    !matches!(widgets::welcome(tty, theme, &intro), Flow::Abort)
}

/// One step in the wizard spine. The visible set is profile-dependent
/// ([`visible_steps`]) so a workstation never walks a pairing screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Profile,
    Hardware,
    Components,
    Wifi,
    Name,
    Pair,
    Review,
}

impl Step {
    /// The rail label for this step.
    fn label(self) -> &'static str {
        match self {
            Step::Profile => "Role",
            Step::Hardware => "Hardware",
            Step::Components => "Features",
            Step::Wifi => "Wi-Fi",
            Step::Name => "Name",
            Step::Pair => "Pairing",
            Step::Review => "Review",
        }
    }
}

/// The steps shown for the chosen profile, in order. Pairing is drone /
/// ground-station only — a workstation runs Mission Control, it does not pair to
/// it — so it is absent from the walk and from the rail count for that profile.
fn visible_steps(args: &Args) -> Vec<Step> {
    let local_only = matches!(
        args.profile.as_deref(),
        Some("workstation") | Some("compute")
    );
    let mut steps = vec![
        Step::Profile,
        Step::Hardware,
        Step::Components,
        Step::Wifi,
        Step::Name,
    ];
    if !local_only {
        steps.push(Step::Pair);
    }
    steps.push(Step::Review);
    steps
}

/// Walk the visible steps with Back navigation. The visible set is recomputed
/// each turn from the current profile, so choosing a profile reshapes the rail
/// and the remaining steps. Returns when the operator finishes at review or
/// cancels.
pub fn run_stages(
    tty: &mut Tty,
    theme: &Theme,
    args: &mut Args,
    hw: &mut hw::HardwareProbe,
    extras: &mut WizardExtras,
    collected: &mut Collected,
) -> Outcome {
    let mut i = 0usize;
    loop {
        let steps = visible_steps(args);
        // Re-clamp in case a profile change shrank the list under the cursor.
        if i >= steps.len() {
            i = steps.len() - 1;
        }
        let step = steps[i];
        tty.set_chrome(i + 1, steps.len(), step.label());
        // Review is terminal-ish: it finishes, cancels, steps back, or jumps to
        // any earlier answer.
        if step == Step::Review {
            match review_stage(tty, theme, args, extras, collected) {
                ReviewNav::Finish => return Outcome::Completed,
                ReviewNav::Abort => return Outcome::Canceled,
                ReviewNav::Back => i = i.saturating_sub(1),
                ReviewNav::JumpTo(target) => {
                    if let Some(pos) = steps.iter().position(|s| *s == target) {
                        i = pos;
                    }
                }
            }
            continue;
        }
        let nav = match step {
            Step::Profile => profile_stage(tty, theme, args),
            Step::Hardware => hardware_stage(tty, theme, args, hw),
            Step::Components => components_stage(tty, theme, args, hw, extras),
            Step::Wifi => wifi_stage(tty, theme, collected),
            Step::Name => name_stage(tty, theme, args),
            Step::Pair => pair_stage(tty, theme, args),
            Step::Review => unreachable!("review is handled above"),
        };
        match nav {
            Nav::Next => {
                if i + 1 < steps.len() {
                    i += 1;
                }
            }
            Nav::Back => i = i.saturating_sub(1),
            Nav::Abort => return Outcome::Canceled,
        }
    }
}

// ── stage: profile ────────────────────────────────────────────────────────

/// The result of one join attempt.
enum JoinOutcome {
    Connected,
    Retry,
    Skip,
    Abort,
}

/// The review screen's action.
enum ReviewNav {
    Finish,
    Back,
    Abort,
    /// Jump straight to a chosen earlier step to change its answer.
    JumpTo(Step),
}

/// Let the operator pick any earlier answer to change, instead of stepping back
/// one screen at a time. Returns the chosen step, or `None` if they backed out
/// of the picker.
fn change_answer(tty: &mut Tty, theme: &Theme, args: &Args) -> Option<Step> {
    let steps: Vec<Step> = visible_steps(args)
        .into_iter()
        .filter(|s| *s != Step::Review)
        .collect();
    let choices: Vec<Choice> = steps
        .iter()
        .map(|s| Choice::new(s.label(), s.label(), None))
        .collect();
    match select_list(tty, theme, "review", "Which answer to change?", &choices, 0) {
        Flow::Value(i) => steps.get(i).copied(),
        Flow::Back | Flow::Abort => None,
    }
}

#[cfg(test)]
mod tests;
