//! The cockpit's quick actions.
//!
//! Each action is a labelled shell-out to an existing `ados` (or `systemctl`)
//! command. Running one leaves the alt screen so the command's own output — and
//! any sudo prompt or its own confirmation — is visible, then the cockpit is
//! restored. The action set is intentionally small and reuses the CLI verbs
//! rather than duplicating their logic or opening a write path to the agent.

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
    /// The program to run and its arguments.
    pub program: &'static str,
    pub args: &'static [&'static str],
}

/// The quick actions, in overlay order. The first three carry direct hotkeys.
pub const ACTIONS: &[Action] = &[
    Action {
        key: Some('d'),
        short: "driver",
        label: "Install / repair RTL driver",
        desc: "Build the RTL8812EU WFB kernel driver",
        confirm: true,
        program: "ados",
        args: &["radio", "install-driver"],
    },
    Action {
        key: Some('p'),
        short: "pair",
        label: "Pair",
        desc: "Show Mission Control pairing info",
        confirm: false,
        program: "ados",
        args: &["pair"],
    },
    Action {
        key: Some('l'),
        short: "logs",
        label: "Logs",
        desc: "Follow the agent logs",
        confirm: false,
        program: "ados",
        args: &["logs", "tail"],
    },
    Action {
        key: None,
        short: "",
        label: "Radio status",
        desc: "Show the WFB radio link",
        confirm: false,
        program: "ados",
        args: &["radio", "status"],
    },
    Action {
        key: None,
        short: "",
        label: "Update agent",
        desc: "Update the agent to the latest",
        confirm: true,
        program: "ados",
        args: &["update"],
    },
    Action {
        key: None,
        short: "",
        label: "Restart radio",
        desc: "Restart the WFB radio service",
        confirm: true,
        program: "sudo",
        args: &["systemctl", "restart", "ados-wfb"],
    },
    Action {
        key: None,
        short: "",
        label: "Unpair",
        desc: "Release this agent's pairing",
        confirm: true,
        program: "ados",
        args: &["unpair"],
    },
    Action {
        key: None,
        short: "",
        label: "Reboot host",
        desc: "Reboot this device",
        confirm: true,
        program: "sudo",
        args: &["systemctl", "reboot"],
    },
];
