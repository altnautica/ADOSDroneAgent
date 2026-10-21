//! The on-box quick actions.
//!
//! Each action is a labelled shell-out to an existing `ados` (or `systemctl`)
//! command. Running one leaves the alt screen so the command's own output, and
//! any sudo prompt or its own confirmation, is visible, then the UI is
//! restored. Actions run only from the actions overlay and the palette, and
//! only against the local agent: on a remote target they would run on the
//! workstation instead.

/// What an action runs.
pub enum ShellCommand {
    /// A fixed program and argument list.
    Run(&'static str, &'static [&'static str]),
    /// `systemctl restart` of the radio unit this node's profile actually runs.
    RestartRadio,
}

/// One quick action.
pub struct Action {
    /// The label shown in the actions overlay and the palette.
    pub label: &'static str,
    /// A one-line description shown beside the label in the overlay.
    pub desc: &'static str,
    /// Ask for a y/N confirmation before running (destructive or long-running).
    pub confirm: bool,
    pub command: ShellCommand,
}

/// Shown instead of running an update while the vehicle reports armed. An update
/// rebuilds and restarts the agent, which drops the MAVLink router, video and
/// radio mid-flight.
pub const UPDATE_REFUSED_ARMED: &str =
    "Update refused: the vehicle reports ARMED. Disarm first; an update restarts \
     the MAVLink router, video and radio.";

/// Shown instead of restarting the radio while the node's profile is unknown:
/// a drone and a ground station run different radio units, and restarting the
/// wrong one starts a unit the installer disabled on this profile.
pub const RADIO_PROFILE_UNKNOWN: &str =
    "Restart radio refused: the agent has not reported this node's profile yet, \
     so the radio unit to restart is unknown.";

/// The refusal on a Visor, which carries no long-range radio.
pub const RADIO_NOT_ON_VISOR: &str = "Restart radio refused: no radio on a Visor.";

/// The radio unit a profile runs: the drone's transmit unit or the ground
/// station's receive unit. `None` for any other (or an unreported) profile,
/// and for a Visor, which has no radio.
pub fn radio_unit(profile: &str) -> Option<&'static str> {
    match profile {
        "drone" => Some("ados-wfb"),
        "ground_station" => Some("ados-wfb-rx"),
        _ => None,
    }
}

impl Action {
    /// The program and arguments to run on a node with this profile, or why the
    /// action cannot run there.
    pub fn argv(
        &self,
        profile: Option<&str>,
    ) -> Result<(&'static str, Vec<&'static str>), &'static str> {
        match self.command {
            ShellCommand::Run(program, args) => Ok((program, args.to_vec())),
            ShellCommand::RestartRadio => {
                if profile == Some("visor") {
                    return Err(RADIO_NOT_ON_VISOR);
                }
                let unit = profile.and_then(radio_unit).ok_or(RADIO_PROFILE_UNKNOWN)?;
                Ok(("sudo", vec!["systemctl", "restart", unit]))
            }
        }
    }

    /// Whether this is the agent update entry.
    pub fn is_update(&self) -> bool {
        matches!(self.command, ShellCommand::Run("ados", args) if args.first() == Some(&"update"))
    }
}

/// What the UI knows about the vehicle's arm state when an update is asked
/// for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArmView {
    /// A fresh snapshot reports disarmed.
    Disarmed,
    /// A fresh snapshot reports armed.
    Armed,
    /// No arm state, or a snapshot too old to trust.
    Unknown,
}

impl ArmView {
    /// The view from the latest snapshot's `armed` field and whether the
    /// snapshot is stale. A stale snapshot proves nothing, whatever it says.
    pub fn from_snapshot(armed: Option<bool>, stale: bool) -> Self {
        match (armed, stale) {
            (_, true) | (None, _) => ArmView::Unknown,
            (Some(true), false) => ArmView::Armed,
            (Some(false), false) => ArmView::Disarmed,
        }
    }
}

/// The note the y/N prompt carries when the arm state is unknown.
pub const ARM_STATE_UNKNOWN: &str = "arm state unknown";

/// The y/N confirmation line for `label`, naming an unknown arm state so the
/// operator decides knowing the UI could not confirm the vehicle is disarmed.
pub fn confirm_prompt(label: &str, arm: Option<ArmView>) -> String {
    match arm {
        Some(ArmView::Unknown) => format!("{label} ({ARM_STATE_UNKNOWN}) — proceed? [y/N] "),
        _ => format!("{label} — proceed? [y/N] "),
    }
}

/// Resolve an operator's update request to the confirming "Update agent" entry
/// of [`ACTIONS`] (y/N prompt, no `--yes`), or refuse it while the vehicle
/// reports armed. An unknown arm state is not a refusal here: the y/N prompt,
/// which then names the unknown state, still stands between the key and the
/// upgrade, and the installer applies its own armed interlock.
pub fn update_request(arm: ArmView) -> Result<&'static Action, &'static str> {
    if arm == ArmView::Armed {
        return Err(UPDATE_REFUSED_ARMED);
    }
    Ok(ACTIONS
        .iter()
        .find(|a| a.is_update())
        .expect("ACTIONS carries the update entry"))
}

/// The quick actions, in overlay order.
pub const ACTIONS: &[Action] = &[
    Action {
        label: "Install RTL driver",
        desc: "Build the RTL8812EU WFB kernel driver if missing",
        confirm: true,
        command: ShellCommand::Run("ados", &["radio", "install-driver"]),
    },
    Action {
        label: "Radio status",
        desc: "Show the WFB radio link",
        confirm: false,
        command: ShellCommand::Run("ados", &["radio", "status"]),
    },
    Action {
        label: "Update agent",
        desc: "Update the agent to the latest",
        confirm: true,
        command: ShellCommand::Run("ados", &["update"]),
    },
    Action {
        label: "Restart radio",
        desc: "Restart this node's WFB radio service",
        confirm: true,
        command: ShellCommand::RestartRadio,
    },
    Action {
        label: "Reboot host",
        desc: "Reboot this device",
        confirm: true,
        command: ShellCommand::Run("sudo", &["systemctl", "reboot"]),
    },
];

/// The index of the "Reboot host" action (the System tab's local reboot).
pub fn reboot_index() -> usize {
    ACTIONS
        .iter()
        .position(|a| a.label == "Reboot host")
        .expect("ACTIONS carries the reboot entry")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_overlay_lists_exactly_the_on_box_actions() {
        let labels: Vec<_> = ACTIONS.iter().map(|a| a.label).collect();
        assert_eq!(
            labels,
            ["Install RTL driver", "Radio status", "Update agent", "Restart radio", "Reboot host"]
        );
        assert_eq!(ACTIONS[reboot_index()].label, "Reboot host");
    }

    #[test]
    fn an_update_request_is_confirmed_and_refused_while_armed() {
        let a = update_request(ArmView::Disarmed).expect("disarmed runs");
        assert!(a.confirm, "the update always asks y/N");
        assert_eq!(
            a.argv(None).unwrap(),
            ("ados", vec!["update"]),
            "never the non-interactive --yes form"
        );
        assert!(update_request(ArmView::Unknown).is_ok_and(|a| a.confirm));
        assert_eq!(
            update_request(ArmView::Armed).err(),
            Some(UPDATE_REFUSED_ARMED)
        );
    }

    #[test]
    fn a_stale_snapshot_is_an_unknown_arm_state_in_the_prompt() {
        assert_eq!(
            ArmView::from_snapshot(Some(false), false),
            ArmView::Disarmed
        );
        assert_eq!(ArmView::from_snapshot(Some(true), false), ArmView::Armed);
        assert_eq!(ArmView::from_snapshot(None, false), ArmView::Unknown);
        // A stale snapshot proves nothing, whichever way it reads.
        assert_eq!(ArmView::from_snapshot(Some(false), true), ArmView::Unknown);
        assert_eq!(ArmView::from_snapshot(Some(true), true), ArmView::Unknown);

        let unknown = confirm_prompt("Update agent", Some(ArmView::Unknown));
        assert!(unknown.contains(ARM_STATE_UNKNOWN), "{unknown}");
        assert!(unknown.ends_with("[y/N] "));
        let known = confirm_prompt("Update agent", Some(ArmView::Disarmed));
        assert!(!known.contains(ARM_STATE_UNKNOWN), "{known}");
        assert_eq!(
            confirm_prompt("Reboot host", None),
            "Reboot host — proceed? [y/N] "
        );
    }

    #[test]
    fn restart_radio_restarts_the_unit_the_profile_runs() {
        let restart = ACTIONS
            .iter()
            .find(|a| a.label == "Restart radio")
            .expect("ACTIONS carries the radio restart");
        assert!(restart.confirm);
        assert_eq!(
            restart.argv(Some("ground_station")).unwrap(),
            ("sudo", vec!["systemctl", "restart", "ados-wfb-rx"]),
            "a ground station receives on ados-wfb-rx; ados-wfb is disabled there"
        );
        assert_eq!(
            restart.argv(Some("drone")).unwrap(),
            ("sudo", vec!["systemctl", "restart", "ados-wfb"])
        );
        assert_eq!(restart.argv(None), Err(RADIO_PROFILE_UNKNOWN));
        assert_eq!(restart.argv(Some("?")), Err(RADIO_PROFILE_UNKNOWN));
        assert_eq!(restart.argv(Some("visor")), Err(RADIO_NOT_ON_VISOR));
        assert_eq!(radio_unit("visor"), None);
    }
}
