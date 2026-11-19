//! The closing screen: success card + first-run guide, or failure panel.
//!
//! Plain renderers print [`plain_lines`] verbatim. The rich renderer wraps the
//! same information in a styled box ([`rich_lines`]), so the *content* is
//! identical whether or not the terminal supports color. [`SummaryData`]
//! implements [`SummaryView`], which is how the shared renderers draw it.

use std::collections::VecDeque;

use ados_term::progress::SummaryView;
#[cfg(test)]
use ados_term::render::visible_width;
use ados_term::theme::Theme;

mod rich;
pub use rich::{fullscreen_grid, rich_lines};

/// The data the renderer needs to draw the final success card or failure panel.
/// Assembled by the installer after the graph run; the renderer combines it with
/// its own buffered log lines for the failure panel.
#[derive(Debug, Clone)]
pub struct SummaryData {
    /// `ok` | `degraded` | `failed`.
    pub status: String,
    /// Installed agent version, or `unknown`.
    pub version: String,
    /// `drone` | `ground_station` | `workstation` | `compute` | `visor`.
    pub profile: String,
    /// Detected board model.
    pub board: String,
    /// The 12-hex device id.
    pub device_id: String,
    /// Hostname. Drives the `<host>.local` line of the card's reach block, which
    /// is gated on the name being resolvable — see `console_urls`.
    pub hostname: String,
    /// Non-loopback IPv4 addresses the box owns, in interface order. The
    /// success card lists one reach URL per entry so the console stays
    /// reachable even when `<host>.local` mDNS does not resolve.
    pub lan_ips: Vec<String>,
    /// Whether the agent has pairing material on disk.
    pub paired: bool,
    /// The access point's SSID and passphrase, on a profile that runs one.
    ///
    /// Shown because the passphrase is now generated per unit rather than
    /// being one published default on every box. Nothing in the product
    /// displayed it before, so a generated value would be undiscoverable and
    /// the access point unjoinable — this is the first place an operator can
    /// read it, and it must not be dropped without a replacement.
    pub ap_ssid: Option<String>,
    pub ap_passphrase: Option<String>,
    /// Every step that did not succeed.
    pub failed_steps: Vec<String>,
    /// The subset that were Required (hard failures).
    pub required_failures: Vec<String>,
    /// Why each required step failed (`<step>: <reason>`), named on the failure
    /// card: a refusal the operator acts on must not read as a bare step name.
    pub failure_reasons: Vec<String>,
    /// Provisioning that is staged but needs a reboot to take effect (a camera
    /// or display overlay, an I2C dtparam), one reason per entry.
    ///
    /// Rendered as its own block: the provisioners have always recorded this in
    /// `/run/ados/reboot-required`, and until the closing card said so an
    /// operator was never told their camera would not work until a reboot.
    pub pending_reboot: Vec<String>,
    /// What kind of machine this install landed on
    /// ([`ados_protocol::host_kind::HostKind`]'s wire string).
    ///
    /// On a personal computer the closing card says so, in place of the access
    /// point block it does not have: the operator needs to know the install
    /// deliberately left their Wi-Fi, display, sleep and updates alone, and how
    /// to reach the console without a LAN advert.
    pub host_kind: String,
    /// How the host kind was decided ("laptop chassis, battery, desktop
    /// session", or `--host`). Empty when there is nothing to explain.
    pub host_basis: String,
    /// Provisioning that needs a reboot which the install deliberately did NOT
    /// perform, because the machine is the owner's own computer.
    ///
    /// Rendered as "Reboot when convenient:" rather than the "Reboot needed to
    /// finish:" block, which is a promise the installer will do it. Same
    /// placement — ahead of the next-step commands — because the hardware it
    /// provisions does not work until the reboot.
    pub deferred_reboot: Vec<String>,
}

impl SummaryView for SummaryData {
    fn plain_lines(&self) -> Vec<String> {
        plain_lines(self)
    }

    fn rich_lines(&self, theme: &Theme, logs: &VecDeque<String>) -> Vec<String> {
        rich_lines(self, theme, logs)
    }

    fn fullscreen_grid(
        &self,
        theme: &Theme,
        journal: &VecDeque<String>,
        cols: usize,
        rows: usize,
    ) -> Vec<String> {
        fullscreen_grid(self, theme, journal, cols, rows)
    }

    fn failed(&self) -> bool {
        self.status == "failed"
    }
}

/// The curated next-step commands shown after a successful install, each with a
/// short plain-language description. Product-facing copy only — no internal
/// tags. Ordered from "learn everything" to "remove". Kept in sync with the CLI's
/// own `ados help` overview (`src/ados/cli/help.py`); both the Linux summary
/// (plain + rich) and the macOS summary render this one list.
pub const NEXT_STEPS: &[(&str, &str)] = &[
    ("ados help", "see all commands"),
    ("ados", "live status dashboard"),
    ("ados status", "status at a glance"),
    ("ados pair", "connect to Mission Control"),
    ("ados update", "update the agent"),
    ("ados logs", "view agent logs"),
    ("ados uninstall", "remove the agent"),
];

/// The local-mode caveat + docs pointer shown in the Notes section.
const NOTE_LOCAL_MODE: &str = "Running in LOCAL mode. Pair over the LAN, no cloud needed.";
const NOTE_DOCS: &str = "Docs: docs.altnautica.com";
/// Reachability guidance: the LAN IP is the most reliable reach and works from
/// the hosted GCS; the `.local` (mDNS) name resolves only from a desktop app.
const NOTE_DIRECT_IP: &str = "Direct IP is most reliable; it works from the hosted GCS too.";
const NOTE_MDNS: &str = "The .local (mDNS) name needs a desktop app on the LAN.";

/// The access-point block, when this profile runs one.
///
/// The passphrase is generated per unit rather than being one published
/// default on every box, so it has to be readable somewhere. This is the first
/// place an operator sees it and the only one that appears without being
/// sought; the OLED, the `ados` dashboard and `ados network ap-passphrase` all
/// require knowing to look. Removing this block does not strand a unit an
/// operator can already reach, but it does mean a fresh install hands back a
/// network nobody has been told the key to.
fn ap_lines(s: &SummaryData) -> Vec<(String, String)> {
    match (&s.ap_ssid, &s.ap_passphrase) {
        (Some(ssid), Some(pass)) => vec![
            ("network".to_string(), ssid.clone()),
            ("passphrase".to_string(), pass.clone()),
        ],
        _ => Vec::new(),
    }
}

/// The reach URLs for the console, most-resolvable first: the `<host>.local`
/// mDNS name, then one `http://<ip>:8080` per discovered LAN address.
///
/// The mDNS form comes from [`ados_protocol::reach::mdns_name_for`], the one
/// definition every surface that hands out a reach name uses. This module used
/// to carry its own copy of both halves of that rule — the
/// empty/`localhost`/`127.*` rejection and the dot-vs-`.local` naming — which is
/// exactly how a card ends up advertising a name the host does not answer to
/// while the pairing probe advertises a different one. `None` means this box has
/// no resolvable mDNS name, and the card then lists only addresses.
fn console_urls(s: &SummaryData) -> Vec<String> {
    // On a personal computer the control surface binds loopback only until the
    // operator shares it, so the LAN and mDNS names are not reach yet. Listing
    // them would be a status surface reporting a false value; the card hands out
    // the address that answers.
    if s.is_personal_host() {
        return vec![PERSONAL_CONSOLE_URL.to_string()];
    }
    let mut urls = Vec::new();
    if let Some(host) = ados_protocol::reach::mdns_name_for(&s.hostname) {
        urls.push(format!("http://{host}:8080"));
    }
    for ip in &s.lan_ips {
        urls.push(format!("http://{ip}:8080"));
    }
    urls
}

/// The headline for the summary, e.g. `ADOS Drone Agent 0.51.4 installed`.
pub fn headline(s: &SummaryData) -> String {
    match s.status.as_str() {
        "failed" => "ADOS Drone Agent install failed".to_string(),
        "degraded" => format!("ADOS Drone Agent {} installed with warnings", s.version),
        _ => format!("ADOS Drone Agent {} installed", s.version),
    }
}

/// Plain, escape-free summary lines (machine-grep-friendly). Used by the plain
/// and quiet renderers, and as the fallback content of the rich card.
pub fn plain_lines(s: &SummaryData) -> Vec<String> {
    let mut out = Vec::new();
    out.push(headline(s));

    if s.status == "failed" {
        if !s.required_failures.is_empty() {
            out.push(format!("  failed step: {}", s.required_failures.join(", ")));
        }
        for reason in &s.failure_reasons {
            out.push(format!("  reason:      {reason}"));
        }
        out.push("  full log:    journalctl -t ados-installer".to_string());
        out.push("  retry:       re-run the install one-liner".to_string());
        return out;
    }

    out.push(format!("  profile: {}    board: {}", s.profile, s.board));
    out.push(format!(
        "  device:  {}    {}",
        s.device_id,
        if s.paired { "paired" } else { "not paired" }
    ));
    if s.status == "degraded" && !s.failed_steps.is_empty() {
        out.push(format!("  warnings: {}", s.failed_steps.join(", ")));
    }

    out.push(String::new());
    out.push("Open the console:".to_string());
    for url in console_urls(s) {
        out.push(format!("  {url}"));
    }

    let ap = ap_lines(s);
    if !ap.is_empty() {
        out.push(String::new());
        out.push("Wi-Fi access point:".to_string());
        for (label, value) in &ap {
            out.push(format!("  {label:<11} {value}"));
        }
        out.push("  This passphrase is unique to this unit. Write it down.".to_string());
    }

    // A ground station on somebody's own computer: say so, and say what was
    // left alone. In place of the access-point block it does not have.
    if s.is_personal_host() {
        out.push(String::new());
        out.push(format!("{PERSONAL_HEADING}:"));
        out.push(format!("  {PERSONAL_REASSURANCE}"));
        out.push("  Only this computer can reach it until you share it.".to_string());
        if !s.host_basis.is_empty() {
            out.push(format!("  detected: {}", s.host_basis));
        }
    }

    // Before "Next steps": a staged overlay means the hardware it provisions is
    // NOT working yet, so it outranks anything the operator might type.
    if !s.pending_reboot.is_empty() {
        out.push(String::new());
        out.push("Reboot needed to finish:".to_string());
        for reason in &s.pending_reboot {
            out.push(format!("  {reason}"));
        }
    }
    // The same fact on a machine the install will not take down: the owner
    // reboots when it suits them.
    if !s.deferred_reboot.is_empty() {
        out.push(String::new());
        out.push("Reboot when convenient:".to_string());
        for reason in &s.deferred_reboot {
            out.push(format!("  {reason}"));
        }
    }

    out.push(String::new());
    out.push("Next steps:".to_string());
    let cmd_w = next_step_cmd_width();
    for &(cmd, desc) in NEXT_STEPS {
        out.push(format!("  {cmd:<cmd_w$}   {desc}"));
    }

    out.push(String::new());
    out.push(NOTE_LOCAL_MODE.to_string());
    out.push(NOTE_DIRECT_IP.to_string());
    out.push(NOTE_MDNS.to_string());
    out.push(NOTE_DOCS.to_string());
    out
}

/// Whether this install landed on somebody's own computer.
impl SummaryData {
    /// True on a personal computer, from the persisted wire string.
    pub fn is_personal_host(&self) -> bool {
        self.host_kind == "personal"
    }
}

/// The heading for the personal-computer block, in both card styles. Its
/// wording is the install's promise: the machine is still theirs.
const PERSONAL_HEADING: &str = "Installed as a ground station on this computer";

/// The personal-computer block's second line: exactly what was left alone.
const PERSONAL_REASSURANCE: &str = "Your Wi-Fi, display, sleep and updates are untouched.";

/// Where the console is on a personal computer: loopback only, until the
/// operator shares it from Mission Control.
const PERSONAL_CONSOLE_URL: &str = "http://127.0.0.1:8080";

/// The button and the address, as the operator will see them in Mission
/// Control's connect screen.
const PERSONAL_REACH: &str = "Open Mission Control and choose \"Use this computer as ground station\", or browse to http://127.0.0.1:8080";

/// The command-column width used to align the next-step descriptions (the
/// longest command name).
fn next_step_cmd_width() -> usize {
    NEXT_STEPS
        .iter()
        .map(|(cmd, _)| cmd.chars().count())
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_term::theme::ColorTier;

    fn sample(status: &str) -> SummaryData {
        SummaryData {
            status: status.to_string(),
            version: "0.51.4".to_string(),
            profile: "drone".to_string(),
            board: "Raspberry Pi 4 Model B".to_string(),
            device_id: "17bf646b".to_string(),
            hostname: "skynode".to_string(),
            lan_ips: vec!["192.168.1.42".to_string()],
            paired: true,
            failed_steps: vec![],
            ap_ssid: None,
            ap_passphrase: None,
            required_failures: vec![],
            failure_reasons: vec![],
            pending_reboot: vec![],
            host_kind: "appliance".to_string(),
            host_basis: String::new(),
            deferred_reboot: vec![],
        }
    }

    #[test]
    fn a_failed_install_names_the_reason_of_the_failed_step() {
        // An armed-vehicle refusal must tell the operator why and how to
        // proceed, not only which step failed.
        let mut s = sample("failed");
        s.required_failures = vec!["preflight".to_string()];
        s.failure_reasons = vec![
            "preflight: refusing to upgrade: the vehicle is armed (pass --force-armed)".to_string(),
        ];
        let body = plain_lines(&s).join("\n");
        assert!(body.contains("failed step: preflight"), "{body}");
        assert!(body.contains("the vehicle is armed"), "{body}");
        assert!(body.contains("--force-armed"), "{body}");
    }

    #[test]
    fn a_pending_reboot_is_named_on_the_closing_card() {
        // The provisioners have always recorded this; until the card said so an
        // operator was never told their camera would not work until a reboot.
        let mut s = sample("degraded");
        s.pending_reboot = vec!["camera-overlay radxa-camera-13m-214".to_string()];
        let lines = plain_lines(&s);
        let body = lines.join("\n");
        assert!(body.contains("Reboot needed to finish:"), "{body}");
        assert!(
            body.contains("camera-overlay radxa-camera-13m-214"),
            "{body}"
        );
        // It outranks the next-step commands: the hardware is not working yet.
        let reboot_at = lines
            .iter()
            .position(|l| l.contains("Reboot needed to finish:"))
            .unwrap();
        let next_at = lines.iter().position(|l| l == "Next steps:").unwrap();
        assert!(reboot_at < next_at);
    }

    #[test]
    fn no_pending_reboot_renders_no_reboot_block() {
        let body = plain_lines(&sample("ok")).join("\n");
        assert!(!body.contains("Reboot needed"));
    }

    /// A ground station installed on somebody's own computer must say so, say
    /// what it left alone, and point at the loopback console it actually
    /// serves — with no access-point block, because there is no access point.
    #[test]
    fn a_personal_install_says_what_it_left_alone() {
        let mut s = sample("ok");
        s.profile = "ground_station".to_string();
        s.host_kind = "personal".to_string();
        s.host_basis = "desktop session, laptop chassis, battery, x86_64".to_string();
        let body = plain_lines(&s).join("\n");
        assert!(body.contains(PERSONAL_HEADING), "{body}");
        assert!(
            body.contains("Wi-Fi, display, sleep and updates are untouched"),
            "{body}"
        );
        assert!(body.contains("detected: desktop session"), "{body}");
        assert!(!body.contains("Wi-Fi access point"), "{body}");
        assert!(body.contains("http://127.0.0.1:8080"), "{body}");

        // The same block on the rich card, in its own section style.
        let theme = Theme {
            ascii: false,
            tier: ColorTier::None,
        };
        let card = rich_lines(&s, &theme, &VecDeque::new()).join("\n");
        assert!(card.contains(PERSONAL_HEADING), "{card}");
        assert!(
            card.contains("Wi-Fi, display, sleep and updates are untouched"),
            "{card}"
        );
    }

    /// An appliance card is unchanged by the personal block existing.
    #[test]
    fn an_appliance_install_renders_no_personal_block() {
        let mut s = sample("ok");
        s.profile = "ground_station".to_string();
        let body = plain_lines(&s).join("\n");
        assert!(!body.contains(PERSONAL_HEADING), "{body}");
    }

    /// A reboot the install deliberately did not perform reads as the owner's
    /// choice, never as "needed to finish" — which promises the installer will
    /// do it.
    #[test]
    fn a_deferred_reboot_is_the_owners_choice() {
        let mut s = sample("degraded");
        s.profile = "ground_station".to_string();
        s.host_kind = "personal".to_string();
        s.deferred_reboot = vec!["camera overlay staged".to_string()];
        let lines = plain_lines(&s);
        let body = lines.join("\n");
        assert!(body.contains("Reboot when convenient:"), "{body}");
        assert!(!body.contains("Reboot needed to finish:"), "{body}");
        assert!(body.contains("camera overlay staged"), "{body}");
        // Still ahead of the next-step commands: the hardware does not work yet.
        let reboot_at = lines
            .iter()
            .position(|l| l.contains("Reboot when convenient:"))
            .unwrap();
        let next_at = lines.iter().position(|l| l == "Next steps:").unwrap();
        assert!(reboot_at < next_at);

        let theme = Theme {
            ascii: false,
            tier: ColorTier::None,
        };
        let card = rich_lines(&s, &theme, &VecDeque::new()).join("\n");
        assert!(card.contains("Reboot when convenient"), "{card}");
    }

    #[test]
    fn ok_summary_leads_with_mdns_and_lan_ip_and_new_steps() {
        let lines = plain_lines(&sample("ok"));
        assert!(lines[0].contains("0.51.4 installed"));
        // Reach block carries the `.local` mDNS host AND the LAN IP so the
        // console is reachable even when mDNS does not resolve.
        assert!(lines.iter().any(|l| l.contains("Open the console:")));
        assert!(lines
            .iter()
            .any(|l| l.contains("http://skynode.local:8080")));
        assert!(lines.iter().any(|l| l.contains("http://192.168.1.42:8080")));
        // The `.local` host comes before the LAN IP in the reach block.
        let mdns_at = lines.iter().position(|l| l.contains("skynode.local:8080"));
        let ip_at = lines.iter().position(|l| l.contains("192.168.1.42:8080"));
        assert!(mdns_at < ip_at, "mDNS host must lead the reach block");
        // The curated next-step commands, including `ados help`, `ados update`,
        // and `ados logs`.
        assert!(lines.iter().any(|l| l.contains("ados help")));
        assert!(lines.iter().any(|l| l.contains("ados update")));
        assert!(lines.iter().any(|l| l.contains("ados logs")));
        assert!(lines.iter().any(|l| l.contains("ados uninstall")));
        // No bare localhost reach line, and journalctl is off the success path.
        assert!(!lines.iter().any(|l| l.contains("localhost")));
        assert!(!lines.iter().any(|l| l.contains("journalctl")));
    }

    #[test]
    fn rich_card_reach_block_has_mdns_and_lan_ip_but_never_localhost() {
        let theme = Theme {
            ascii: false,
            tier: ColorTier::None,
        };
        let logs = VecDeque::new();
        let joined = rich_lines(&sample("ok"), &theme, &logs).join("\n");
        assert!(joined.contains("skynode.local:8080"));
        assert!(joined.contains("192.168.1.42:8080"));
        assert!(joined.contains("ados help"));
        assert!(!joined.contains("localhost"));
    }

    #[test]
    fn fullscreen_grid_centers_the_card_full_screen() {
        let theme = Theme {
            ascii: false,
            tier: ColorTier::None,
        };
        let journal = VecDeque::new();
        let grid = fullscreen_grid(&sample("ok"), &theme, &journal, 100, 30);
        // Exactly one full-width line per row so the background fills the screen.
        assert_eq!(grid.len(), 30);
        assert!(grid.iter().all(|l| visible_width(l) == 100));
        // Carries the card content + the dismiss hint.
        let joined = grid.join("\n");
        assert!(joined.contains("ADOS Drone Agent"));
        assert!(joined.contains("skynode.local:8080"));
        assert!(joined.contains("Press Enter to finish"));
    }

    /// The install is the one moment the key appears unprompted. Other surfaces
    /// exist (the OLED, the `ados` dashboard, `ados network ap-passphrase`),
    /// so losing this block is not fatal, but it turns a fresh install into one
    /// that finishes by naming a network and withholding its key.
    #[test]
    fn the_access_point_credentials_are_shown_when_there_is_an_access_point() {
        let mut s = sample("ok");
        s.ap_ssid = Some("ADOS-GS-9F2C".to_string());
        s.ap_passphrase = Some("KM7QRT4XPN29".to_string());

        let plain = plain_lines(&s).join("\n");
        assert!(plain.contains("ADOS-GS-9F2C"), "the network must be named");
        assert!(
            plain.contains("KM7QRT4XPN29"),
            "nothing else in the product shows this value"
        );
        assert!(
            plain.contains("unique to this unit"),
            "the operator has to know it is not the same on every box"
        );
    }

    #[test]
    fn no_access_point_block_on_a_profile_that_runs_none() {
        let s = sample("ok");
        assert!(s.ap_ssid.is_none());
        let plain = plain_lines(&s).join("\n");
        assert!(
            !plain.contains("Wi-Fi access point"),
            "a drone with no access point must not show an empty credentials block"
        );
    }

    #[test]
    fn a_half_known_access_point_shows_nothing_rather_than_a_blank_field() {
        // A network name with no passphrase would send the operator to a
        // network they cannot join, with no hint why.
        let mut s = sample("ok");
        s.ap_ssid = Some("ADOS-GS-9F2C".to_string());
        s.ap_passphrase = None;
        let plain = plain_lines(&s).join("\n");
        assert!(!plain.contains("Wi-Fi access point"));
    }

    #[test]
    fn console_urls_drops_the_mdns_line_when_only_ips_resolve() {
        let mut s = sample("ok");
        s.hostname = "localhost".to_string();
        let urls = console_urls(&s);
        assert_eq!(urls, vec!["http://192.168.1.42:8080".to_string()]);
    }

    #[test]
    fn failed_summary_points_at_the_log_and_failed_step() {
        let mut s = sample("failed");
        s.required_failures = vec!["systemd".to_string()];
        s.failed_steps = vec!["systemd".to_string()];
        let lines = plain_lines(&s);
        assert!(lines[0].contains("failed"));
        assert!(lines.iter().any(|l| l.contains("systemd")));
        assert!(lines
            .iter()
            .any(|l| l.contains("journalctl -t ados-installer")));
    }
}
