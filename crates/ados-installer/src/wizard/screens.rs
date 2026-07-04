//! The wizard's step spine: one screen per decision, with Back support.
//!
//! Each stage shows the auto-detected default, takes one primary action, and
//! writes its result into `Args` / `WizardExtras`. The stages are walked in
//! order with Esc going back a step; the review screen finishes or re-enters an
//! earlier answer. Every screen reads through [`crate::ui::tty::Tty`] and
//! renders through [`crate::ui::theme`], so it degrades cleanly to no-color and
//! ASCII terminals.

use crate::cli::Args;
use crate::steps::config_identity::slugify_hostname;
use crate::ui::theme::Theme;
use crate::ui::tty::Tty;
use crate::wizard::render::wordmark;
use crate::wizard::widgets::{
    self, ack_card, checklist, confirm_card, insert_hostname_char, insert_pair_char,
    insert_region_char, insert_ssid_char, paint_board, password_input, select_list, summary_select,
    Ack, BoardItem, CheckItem, Choice, Flow, ItemState, WifiPick, WifiRow,
};
use crate::wizard::{hw, wifi, WizardExtras};

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

/// Print the one-time greeting above the first card.
pub fn greet(tty: &mut Tty, theme: &Theme) {
    let intro = vec![
        format!(
            "{}  {}",
            theme.accent(wordmark(theme)),
            theme.bold("Welcome to ADOS")
        ),
        theme.dim("Let's set up this device. It takes about 3 minutes."),
        String::new(),
    ];
    tty.paint(&intro);
    tty.commit();
}

/// Walk the stages in order with Back navigation. Returns when the operator
/// finishes at the review screen or cancels.
pub fn run_stages(
    tty: &mut Tty,
    theme: &Theme,
    args: &mut Args,
    hw: &mut hw::HardwareProbe,
    extras: &mut WizardExtras,
    collected: &mut Collected,
) -> Outcome {
    // Stage indices: 0 profile, 1 hardware, 2 components, 3 wifi, 4 name,
    // 5 pair, 6 review.
    let last = 6usize;
    let mut i = 0usize;
    loop {
        let nav = match i {
            0 => profile_stage(tty, theme, args),
            1 => hardware_stage(tty, theme, hw),
            2 => components_stage(tty, theme, args, hw, extras),
            3 => wifi_stage(tty, theme, collected),
            4 => name_stage(tty, theme, args),
            5 => pair_stage(tty, theme, args),
            _ => match review_stage(tty, theme, args, extras, collected) {
                ReviewNav::Finish => return Outcome::Completed,
                ReviewNav::Back => Nav::Back,
                ReviewNav::Abort => Nav::Abort,
            },
        };
        match nav {
            Nav::Next => i = (i + 1).min(last),
            Nav::Back => i = i.saturating_sub(1),
            Nav::Abort => return Outcome::Canceled,
        }
    }
}

// ── stage: profile ────────────────────────────────────────────────────────

fn profile_stage(tty: &mut Tty, theme: &Theme, args: &mut Args) -> Nav {
    let choices = vec![
        Choice::new(
            "drone",
            "This flies (Drone)",
            Some("Camera, radio, and flight computer on the aircraft."),
        ),
        Choice::new(
            "ground_station",
            "This receives on the ground (Ground station)",
            Some("The link between you and the drone."),
        ),
        Choice::new(
            "workstation",
            "Your computer (Workstation)",
            Some("Runs the app and the heavy processing."),
        ),
    ];
    let default_idx = match args.profile.as_deref() {
        Some("ground_station") => 1,
        Some("workstation") | Some("compute") => 2,
        _ => 0,
    };
    match select_list(
        tty,
        theme,
        SETUP,
        "What is this device?",
        &choices,
        default_idx,
    ) {
        Flow::Value(i) => {
            args.profile = Some(choices[i].id.clone());
            Nav::Next
        }
        Flow::Back => Nav::Back,
        Flow::Abort => Nav::Abort,
    }
}

// ── stage: hardware check ──────────────────────────────────────────────────

fn hardware_stage(tty: &mut Tty, theme: &Theme, hw: &mut hw::HardwareProbe) -> Nav {
    loop {
        let body = vec![
            hw_row(
                theme,
                "Flight controller",
                hw.fc.is_some(),
                hw.fc.as_deref(),
            ),
            hw_row(theme, "Long-range radio", hw.radio, None),
            hw_row(theme, "Camera", hw.camera.is_some(), hw.camera.as_deref()),
        ];
        match ack_card(tty, theme, SETUP, "Checking the hardware", &body, true) {
            Flow::Value(Ack::Continue) => return Nav::Next,
            Flow::Value(Ack::Rescan) => {
                *hw = hw::probe();
            }
            Flow::Back => return Nav::Back,
            Flow::Abort => return Nav::Abort,
        }
    }
}

/// One hardware-check row: a green tick or a dim dash, a fixed-width label, and
/// a dim detail (`found on <path>` / `found` / `not detected`).
fn hw_row(theme: &Theme, label: &str, present: bool, path: Option<&str>) -> String {
    let mark = if present {
        theme.ok(theme.glyph_ok())
    } else {
        theme.dim(if theme.ascii { "-" } else { "—" })
    };
    let detail = match (present, path) {
        (true, Some(p)) => format!("found on {p}"),
        (true, None) => "found".to_string(),
        (false, _) => "not detected".to_string(),
    };
    format!(" {mark} {:<18}{}", label, theme.dim(&detail))
}

// ── stage: components ──────────────────────────────────────────────────────

fn components_stage(
    tty: &mut Tty,
    theme: &Theme,
    args: &mut Args,
    hw: &hw::HardwareProbe,
    extras: &mut WizardExtras,
) -> Nav {
    let profile = args.profile.clone().unwrap_or_else(|| "drone".to_string());
    let radio_default = profile == "drone" || profile == "ground_station" || hw.radio;

    let mut items = vec![CheckItem {
        id: "radio".into(),
        label: "Long-range radio link".into(),
        benefit: "Fly and stream far past Wi-Fi range.".into(),
        checked: radio_default,
        locked: false,
    }];
    if profile == "drone" {
        items.push(CheckItem {
            id: "camera".into(),
            label: "Camera video".into(),
            benefit: "See a live picture from the drone.".into(),
            checked: hw.camera.is_some(),
            locked: false,
        });
    }
    items.push(CheckItem {
        id: "display".into(),
        label: "Onboard screen".into(),
        benefit: "Show status on a small attached display.".into(),
        checked: profile == "ground_station",
        locked: false,
    });
    items.push(CheckItem {
        id: "cloud".into(),
        label: "Reach it from anywhere".into(),
        benefit: "Connect over the internet, not just at home.".into(),
        checked: false,
        locked: false,
    });

    match checklist(
        tty,
        theme,
        SETUP,
        "What should this device do? (recommended are on)",
        items,
    ) {
        Flow::Value(result) => {
            for it in &result {
                match it.id.as_str() {
                    "radio" => args.no_rtl_driver = !it.checked,
                    "camera" => {
                        args.camera = Some(if it.checked { "auto" } else { "none" }.to_string())
                    }
                    "display" => {
                        args.display = Some(if it.checked { "auto" } else { "none" }.to_string())
                    }
                    "cloud" => extras.cloud_from_anywhere = it.checked,
                    _ => {}
                }
            }
            Nav::Next
        }
        Flow::Back => Nav::Back,
        Flow::Abort => Nav::Abort,
    }
}

// ── stage: Wi-Fi ───────────────────────────────────────────────────────────

fn wifi_stage(tty: &mut Tty, theme: &Theme, collected: &mut Collected) -> Nav {
    // Never reconfigure the radio the operator's session rides on.
    if wifi::session_rides_wifi() {
        return match ack_card(
            tty,
            theme,
            WIFI,
            "Wi-Fi is already in use",
            &[
                theme.dim("You're connected over Wi-Fi for this setup."),
                theme.dim("Changing it now could drop the connection."),
                theme.dim("You can set up Wi-Fi later from the dashboard."),
            ],
            false,
        ) {
            Flow::Value(_) => Nav::Next,
            Flow::Back => Nav::Back,
            Flow::Abort => Nav::Abort,
        };
    }

    let on_wifi = wifi::currently_on_wifi();
    let detail = if on_wifi {
        vec![theme.dim("You're already on Wi-Fi. You can switch networks if you want.")]
    } else {
        vec![theme.dim("Connect to Wi-Fi so the device keeps working after you unplug the cable.")]
    };
    match confirm_card(
        tty,
        theme,
        WIFI,
        "Set up Wi-Fi now?",
        &detail,
        "Set up Wi-Fi",
        "Skip",
        !on_wifi,
    ) {
        Flow::Value(false) => return Nav::Next,
        Flow::Value(true) => {}
        Flow::Back => return Nav::Back,
        Flow::Abort => return Nav::Abort,
    }

    let iface = match wifi::management_wifi_iface() {
        Some(i) => i,
        None => {
            return match ack_card(
                tty,
                theme,
                WIFI,
                "No Wi-Fi adapter found",
                &[
                    theme.dim("This device has no Wi-Fi radio the setup can use."),
                    theme.dim("Stay on the wired cable, or add a Wi-Fi adapter and rerun setup."),
                ],
                false,
            ) {
                Flow::Value(_) => Nav::Next,
                Flow::Back => Nav::Back,
                Flow::Abort => Nav::Abort,
            }
        }
    };

    loop {
        // Scan with a one-line live board.
        paint_board(
            tty,
            theme,
            WIFI,
            &[BoardItem {
                label: "Looking for Wi-Fi networks…".into(),
                state: ItemState::Active,
                detail: None,
            }],
            0,
        );
        let nets = wifi::scan(&iface);

        if nets.is_empty() {
            match ack_card(
                tty,
                theme,
                WIFI,
                "No networks found",
                &[theme.dim("No Wi-Fi networks were seen nearby.")],
                true,
            ) {
                Flow::Value(Ack::Rescan) => continue,
                Flow::Value(Ack::Continue) => return Nav::Next,
                Flow::Back => return Nav::Back,
                Flow::Abort => return Nav::Abort,
            }
        }

        let rows: Vec<WifiRow> = nets
            .iter()
            .map(|n| WifiRow {
                ssid: n.ssid.clone(),
                signal: n.signal,
                secured: n.secured,
                in_use: n.in_use,
            })
            .collect();

        let (ssid, secured, hidden) = match widgets::wifi_picker(tty, theme, &rows) {
            Flow::Value(WifiPick::Rescan) => continue,
            Flow::Value(WifiPick::Network { ssid, secured }) => (ssid, secured, false),
            Flow::Value(WifiPick::Hidden) => match hidden_ssid(tty, theme) {
                Flow::Value(s) => (s, true, true),
                Flow::Back => continue,
                Flow::Abort => return Nav::Abort,
            },
            Flow::Back => return Nav::Back,
            Flow::Abort => return Nav::Abort,
        };

        let password = if secured {
            match password_input(tty, theme, WIFI, &format!("Password for  {ssid}"), 8) {
                Flow::Value(p) => Some(p),
                Flow::Back => continue,
                Flow::Abort => return Nav::Abort,
            }
        } else {
            None
        };

        match join_flow(tty, theme, &iface, &ssid, password.as_deref(), hidden) {
            JoinOutcome::Connected => {
                collected.wifi_ssid = Some(ssid);
                return Nav::Next;
            }
            JoinOutcome::Retry => continue,
            JoinOutcome::Skip => return Nav::Next,
            JoinOutcome::Abort => return Nav::Abort,
        }
    }
}

/// The result of one join attempt.
enum JoinOutcome {
    Connected,
    Retry,
    Skip,
    Abort,
}

/// Prompt for a hidden network's name.
fn hidden_ssid(tty: &mut Tty, theme: &Theme) -> Flow<String> {
    widgets::text_input(
        tty,
        theme,
        WIFI,
        "Enter the network name",
        "",
        "",
        insert_ssid_char,
        |_| None,
        |raw| {
            if raw.trim().is_empty() {
                Some("Type the network name.".to_string())
            } else {
                None
            }
        },
    )
}

/// Connect → verify LAN reachability → persist, with a live board and a final
/// confirmation card that tells the operator it is safe to unplug the cable.
fn join_flow(
    tty: &mut Tty,
    theme: &Theme,
    iface: &str,
    ssid: &str,
    password: Option<&str>,
    hidden: bool,
) -> JoinOutcome {
    let mut items = vec![
        BoardItem {
            label: format!("Connecting to {ssid}…"),
            state: ItemState::Active,
            detail: None,
        },
        BoardItem {
            label: "Checking the connection".into(),
            state: ItemState::Queued,
            detail: None,
        },
        BoardItem {
            label: "Saving for next time".into(),
            state: ItemState::Queued,
            detail: None,
        },
    ];
    paint_board(tty, theme, WIFI, &items, 0);

    if let Err(e) = wifi::connect(iface, ssid, password, hidden) {
        items[0].state = ItemState::Failed;
        items[0].detail = Some(short_reason(&e));
        paint_board(tty, theme, WIFI, &items, 0);
        return retry_prompt(tty, theme);
    }
    items[0].state = ItemState::Ok;
    items[0].detail = Some(format!("on {iface}"));
    items[1].state = ItemState::Active;
    paint_board(tty, theme, WIFI, &items, 0);

    let reach = wifi::verify_lan_reachable(iface);
    if !reach.reachable {
        items[1].state = ItemState::Failed;
        items[1].detail = Some("could not reach your network".into());
        paint_board(tty, theme, WIFI, &items, 0);
        return retry_prompt(tty, theme);
    }
    items[1].state = ItemState::Ok;
    items[1].detail = reach.gateway.as_ref().map(|g| format!("gateway {g}"));
    items[2].state = ItemState::Active;
    paint_board(tty, theme, WIFI, &items, 0);

    wifi::persist(ssid);
    items[2].state = ItemState::Ok;
    paint_board(tty, theme, WIFI, &items, 0);

    // Final confirmation with the safe-to-unplug message (Enter to continue).
    let gw_note = reach
        .gateway
        .as_ref()
        .map(|g| format!("Reached your network (gateway {g})."))
        .unwrap_or_else(|| "Reached your network.".to_string());
    let _ = ack_card(
        tty,
        theme,
        WIFI,
        &format!("Connected to {ssid}"),
        &[
            theme.ok(&format!("{} {gw_note}", theme.glyph_ok())),
            theme.bold("You can unplug the network cable now."),
        ],
        false,
    );
    JoinOutcome::Connected
}

/// Offer another network after a failed join.
fn retry_prompt(tty: &mut Tty, theme: &Theme) -> JoinOutcome {
    match confirm_card(
        tty,
        theme,
        WIFI,
        "That didn't connect. Try another network?",
        &[],
        "Try again",
        "Skip Wi-Fi",
        true,
    ) {
        Flow::Value(true) => JoinOutcome::Retry,
        Flow::Value(false) => JoinOutcome::Skip,
        Flow::Back => JoinOutcome::Retry,
        Flow::Abort => JoinOutcome::Abort,
    }
}

/// Shorten an nmcli error to a single tidy line for the board detail.
fn short_reason(reason: &str) -> String {
    let line = reason.lines().next().unwrap_or(reason).trim();
    let cleaned = line.trim_start_matches("Error:").trim();
    let mut out: String = cleaned.chars().take(48).collect();
    if cleaned.chars().count() > 48 {
        out.push('…');
    }
    if out.is_empty() {
        "could not connect".to_string()
    } else {
        out
    }
}

// ── stage: name ────────────────────────────────────────────────────────────

fn name_stage(tty: &mut Tty, theme: &Theme, args: &mut Args) -> Nav {
    let profile = args.profile.clone().unwrap_or_else(|| "drone".to_string());
    let seed = default_name(&profile);
    match widgets::text_input(
        tty,
        theme,
        SETUP,
        "Name this device",
        &seed,
        "Others reach it at:",
        insert_hostname_char,
        |raw| {
            let slug = slugify_hostname(raw);
            if slug.is_empty() {
                None
            } else {
                Some(format!("{slug}.local"))
            }
        },
        |raw| {
            if slugify_hostname(raw).is_empty() {
                Some("Type at least one letter or number.".to_string())
            } else {
                None
            }
        },
    ) {
        Flow::Value(name) => {
            args.name = Some(name);
            Nav::Next
        }
        Flow::Back => Nav::Back,
        Flow::Abort => Nav::Abort,
    }
}

/// The prefilled device name for a profile (`ados-drone-01`, `ados-ground-01`,
/// `ados-workstation-01`), already hostname-safe.
fn default_name(profile: &str) -> String {
    let short = match profile {
        "ground_station" => "ground",
        "workstation" => "workstation",
        "compute" => "compute",
        _ => "drone",
    };
    format!("ados-{short}-01")
}

// ── stage: pairing ─────────────────────────────────────────────────────────

fn pair_stage(tty: &mut Tty, theme: &Theme, args: &mut Args) -> Nav {
    loop {
        match confirm_card(
            tty,
            theme,
            SETUP,
            "Connect this to Mission Control now?",
            &[
                theme.dim("Add it in Mission Control by its name, no code needed."),
                theme.dim("Or enter a pairing code now. You can also pair later: ados pair"),
            ],
            "Enter a code",
            "Later",
            false,
        ) {
            Flow::Value(false) => return Nav::Next,
            Flow::Value(true) => match widgets::text_input(
                tty,
                theme,
                SETUP,
                "Enter the pairing code",
                "",
                "",
                insert_pair_char,
                |_| None,
                |raw| {
                    if raw.trim().is_empty() {
                        Some("Type the code shown in Mission Control.".to_string())
                    } else {
                        None
                    }
                },
            ) {
                Flow::Value(code) => {
                    args.pair = Some(code);
                    return Nav::Next;
                }
                Flow::Back => continue,
                Flow::Abort => return Nav::Abort,
            },
            Flow::Back => return Nav::Back,
            Flow::Abort => return Nav::Abort,
        }
    }
}

// ── stage: review ──────────────────────────────────────────────────────────

/// The review screen's action.
enum ReviewNav {
    Finish,
    Back,
    Abort,
}

fn review_stage(
    tty: &mut Tty,
    theme: &Theme,
    args: &mut Args,
    extras: &mut WizardExtras,
    collected: &Collected,
) -> ReviewNav {
    loop {
        let summary = review_summary(theme, args, extras, collected);
        let choices = vec![
            Choice::new("finish", "Finish and set up", None),
            Choice::new("change", "Change an answer", None),
            Choice::new("region", "Operating region (advanced)", None),
        ];
        match summary_select(
            tty,
            theme,
            "review",
            "Ready to set up:",
            &summary,
            &choices,
            0,
        ) {
            Flow::Value(0) => return ReviewNav::Finish,
            Flow::Value(1) => return ReviewNav::Back,
            Flow::Value(_) => match region_advanced(tty, theme, extras) {
                Flow::Abort => return ReviewNav::Abort,
                _ => continue,
            },
            Flow::Back => return ReviewNav::Back,
            Flow::Abort => return ReviewNav::Abort,
        }
    }
}

/// Build the review summary lines from the collected answers.
fn review_summary(
    theme: &Theme,
    args: &Args,
    extras: &WizardExtras,
    collected: &Collected,
) -> Vec<String> {
    let profile = args.profile.clone().unwrap_or_else(|| "drone".to_string());
    let name = args.name.clone().unwrap_or_else(|| default_name(&profile));
    let mut rows = vec![
        kv(
            theme,
            "Device",
            &format!("{name}   ({})", friendly_profile(&profile)),
        ),
        kv(theme, "Radio", onoff(!args.no_rtl_driver)),
    ];
    if profile == "drone" {
        rows.push(kv(
            theme,
            "Camera",
            onoff(args.camera.as_deref() == Some("auto")),
        ));
    }
    rows.push(kv(
        theme,
        "Wi-Fi",
        collected.wifi_ssid.as_deref().unwrap_or("skipped"),
    ));
    rows.push(kv(
        theme,
        "Reach",
        if extras.cloud_from_anywhere {
            "From anywhere"
        } else {
            "On my network"
        },
    ));
    rows.push(kv(
        theme,
        "Region",
        extras.region_pinned.as_deref().unwrap_or("Unrestricted"),
    ));
    rows.push(kv(
        theme,
        "Pairing",
        if args.pair.is_some() {
            "code entered"
        } else {
            "add later in Mission Control"
        },
    ));
    rows
}

/// A dim key + value review row.
fn kv(theme: &Theme, key: &str, value: &str) -> String {
    format!("  {}  {value}", theme.dim(&format!("{key:<10}")))
}

/// `on` / `off` for a boolean review row.
fn onoff(on: bool) -> &'static str {
    if on {
        "on"
    } else {
        "off"
    }
}

/// The human profile name for the review + summary.
fn friendly_profile(profile: &str) -> &'static str {
    match profile {
        "ground_station" => "Ground station",
        "workstation" => "Workstation",
        "compute" => "Compute node",
        _ => "Drone",
    }
}

/// The advanced operating-region chooser. Default is unrestricted; a pinned
/// region applies that country's radio rules. The honest, non-compliance-
/// claiming note lives on the unrestricted choice and in the written config.
fn region_advanced(tty: &mut Tty, theme: &Theme, extras: &mut WizardExtras) -> Flow<()> {
    let choices = vec![
        Choice::new(
            "",
            "Unrestricted (recommended)",
            Some("Transmit on your channel at the device's power. You follow the radio rules where you fly."),
        ),
        Choice::new("US", "United States (US)", None),
        Choice::new("IN", "India (IN)", None),
        Choice::new("GB", "United Kingdom (GB)", None),
        Choice::new("AU", "Australia (AU)", None),
        Choice::new("__other", "Other (enter a 2-letter code)", None),
    ];
    let default_idx = match extras.region_pinned.as_deref() {
        None => 0,
        Some("US") => 1,
        Some("IN") => 2,
        Some("GB") => 3,
        Some("AU") => 4,
        Some(_) => 5,
    };
    match select_list(
        tty,
        theme,
        "region",
        "Operating region",
        &choices,
        default_idx,
    ) {
        Flow::Value(i) => {
            match choices[i].id.as_str() {
                "" => extras.region_pinned = None,
                "__other" => match widgets::text_input(
                    tty,
                    theme,
                    "region",
                    "Enter a 2-letter country code",
                    "",
                    "",
                    insert_region_char,
                    |_| None,
                    |raw| {
                        if raw.trim().chars().count() == 2 {
                            None
                        } else {
                            Some("Enter exactly two letters (for example US).".to_string())
                        }
                    },
                ) {
                    Flow::Value(code) => extras.region_pinned = Some(code.to_ascii_uppercase()),
                    Flow::Back => {}
                    Flow::Abort => return Flow::Abort,
                },
                code => extras.region_pinned = Some(code.to_string()),
            }
            Flow::Value(())
        }
        Flow::Back => Flow::Value(()),
        Flow::Abort => Flow::Abort,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_names_are_hostname_safe() {
        assert_eq!(default_name("drone"), "ados-drone-01");
        assert_eq!(default_name("ground_station"), "ados-ground-01");
        assert_eq!(default_name("workstation"), "ados-workstation-01");
        // The seed must already be a valid slug (no change under slugify).
        for p in ["drone", "ground_station", "workstation", "compute"] {
            let seed = default_name(p);
            assert_eq!(
                slugify_hostname(&seed),
                seed,
                "seed not slug-stable: {seed}"
            );
        }
    }

    #[test]
    fn short_reason_trims_and_bounds() {
        assert_eq!(
            short_reason("Error: Secrets were required"),
            "Secrets were required"
        );
        assert_eq!(short_reason(""), "could not connect");
        let long = "x".repeat(80);
        let out = short_reason(&long);
        assert!(out.chars().count() <= 49, "reason not bounded: {out}");
    }

    #[test]
    fn friendly_profile_names() {
        assert_eq!(friendly_profile("drone"), "Drone");
        assert_eq!(friendly_profile("ground_station"), "Ground station");
        assert_eq!(friendly_profile("workstation"), "Workstation");
        assert_eq!(friendly_profile("anything-else"), "Drone");
    }
}
