//! The cockpit's quick actions.
//!
//! Each action is a labelled shell-out to an existing `ados` (or `systemctl`)
//! command. Running one leaves the alt screen so the command's own output — and
//! any sudo prompt or its own confirmation — is visible, then the cockpit is
//! restored. The action set is intentionally small and reuses the CLI verbs
//! rather than duplicating their logic or opening a write path to the agent.

/// What an action runs.
pub enum Command {
    /// A fixed program and argument list.
    Run(&'static str, &'static [&'static str]),
    /// `systemctl restart` of the radio unit this node's profile actually runs.
    RestartRadio,
}

/// One quick action.
pub struct Action {
    /// A direct dashboard hotkey, if any (also listed in the actions overlay).
    pub key: Option<char>,
    /// A short word for the bottom action bar (used only when `key` is set).
    pub short: &'static str,
    /// The full label shown in the actions overlay.
    pub label: &'static str,
    /// A one-line description shown beside the label in the overlay.
    pub desc: &'static str,
    /// Ask for a y/N confirmation before running (destructive or long-running).
    pub confirm: bool,
    pub command: Command,
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

/// The radio unit a profile runs: the drone's transmit unit or the ground
/// station's receive unit. `None` for any other (or an unreported) profile.
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
            Command::Run(program, args) => Ok((program, args.to_vec())),
            Command::RestartRadio => {
                let unit = profile.and_then(radio_unit).ok_or(RADIO_PROFILE_UNKNOWN)?;
                Ok(("sudo", vec!["systemctl", "restart", unit]))
            }
        }
    }

    /// Whether this is the agent update entry.
    pub fn is_update(&self) -> bool {
        matches!(self.command, Command::Run("ados", args) if args.first() == Some(&"update"))
    }
}

/// Resolve an operator's update request to the confirming "Update agent" entry
/// of [`ACTIONS`] (y/N prompt, no `--yes`), or refuse it while `armed` reads
/// true. An unknown arm state is not a refusal: the y/N prompt still stands
/// between the key and the upgrade.
pub fn update_request(armed: Option<bool>) -> Result<&'static Action, &'static str> {
    if armed == Some(true) {
        return Err(UPDATE_REFUSED_ARMED);
    }
    Ok(ACTIONS
        .iter()
        .find(|a| a.is_update())
        .expect("ACTIONS carries the update entry"))
}

/// The quick actions, in overlay order. The first three carry direct hotkeys.
pub const ACTIONS: &[Action] = &[
    Action {
        key: Some('d'),
        short: "driver",
        label: "Install RTL driver",
        desc: "Build the RTL8812EU WFB kernel driver if missing",
        confirm: true,
        command: Command::Run("ados", &["radio", "install-driver"]),
    },
    Action {
        key: Some('p'),
        short: "pair",
        label: "Pair",
        desc: "Show Mission Control pairing info",
        confirm: false,
        command: Command::Run("ados", &["pair"]),
    },
    Action {
        key: Some('l'),
        short: "logs",
        label: "Logs",
        desc: "Follow the agent logs",
        confirm: false,
        command: Command::Run("ados", &["logs", "tail"]),
    },
    Action {
        key: None,
        short: "",
        label: "Radio status",
        desc: "Show the WFB radio link",
        confirm: false,
        command: Command::Run("ados", &["radio", "status"]),
    },
    Action {
        key: None,
        short: "",
        label: "Update agent",
        desc: "Update the agent to the latest",
        confirm: true,
        command: Command::Run("ados", &["update"]),
    },
    Action {
        key: None,
        short: "",
        label: "Restart radio",
        desc: "Restart this node's WFB radio service",
        confirm: true,
        command: Command::RestartRadio,
    },
    Action {
        key: None,
        short: "",
        label: "Unpair",
        desc: "Release this agent's pairing",
        confirm: true,
        command: Command::Run("ados", &["unpair"]),
    },
    Action {
        key: None,
        short: "",
        label: "Reboot host",
        desc: "Reboot this device",
        confirm: true,
        command: Command::Run("sudo", &["systemctl", "reboot"]),
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_update_request_is_confirmed_and_refused_while_armed() {
        let a = update_request(Some(false)).expect("disarmed runs");
        assert!(a.confirm, "the update always asks y/N");
        assert_eq!(
            a.argv(None).unwrap(),
            ("ados", vec!["update"]),
            "never the non-interactive --yes form"
        );
        assert!(update_request(None).is_ok_and(|a| a.confirm));
        assert_eq!(update_request(Some(true)).err(), Some(UPDATE_REFUSED_ARMED));
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
    }
}
