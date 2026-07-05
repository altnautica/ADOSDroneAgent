//! The wizard's step spine: one screen per decision, with Back support.
//!
//! Each stage shows the auto-detected default, takes one primary action, and
//! writes its result into `Args` / `WizardExtras`. The stages are walked in
//! order with Esc going back a step; the review screen finishes or re-enters an
//! earlier answer. Every screen reads through [`crate::ui::tty::Tty`] and
//! renders through [`crate::ui::theme`], so it degrades cleanly to no-color and
//! ASCII terminals.

use crate::cli::Args;
use crate::env;
use crate::steps::config_identity::slugify_hostname;
use crate::ui::theme::Theme;
use crate::ui::tty::Tty;
use crate::wizard::widgets::{
    self, ack_card, checklist, confirm_card, insert_hostname_char, insert_pair_char,
    insert_region_char, insert_ssid_char, paint_board, password_input, select_list, summary_select,
    Ack, BoardItem, CheckItem, Choice, Flow, ItemState, Spin, WifiPick, WifiRow,
};
use crate::wizard::{catalog, hw, wifi, WizardExtras};

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

fn hardware_stage(tty: &mut Tty, theme: &Theme, args: &Args, hw: &mut hw::HardwareProbe) -> Nav {
    let profile = catalog::Profile::from_id(args.profile.as_deref().unwrap_or("drone"));
    loop {
        let body = catalog_body(theme, profile, &hw.sys);
        match ack_card(tty, theme, SETUP, "What's connected", &body, true) {
            Flow::Value(Ack::Continue) => return Nav::Next,
            Flow::Value(Ack::Rescan) => {
                // Re-sweep with a live spinner rather than freezing the screen.
                let board = [BoardItem {
                    label: "Scanning for hardware…".into(),
                    state: ItemState::Active,
                    detail: None,
                }];
                match widgets::run_with_spinner(tty, theme, SETUP, &board, hw::probe) {
                    Spin::Done(fresh) => *hw = fresh,
                    Spin::Aborted => return Nav::Abort,
                }
            }
            Flow::Back => return Nav::Back,
            Flow::Abort => return Nav::Abort,
        }
    }
}

/// Build the hardware-scan body for a profile: the detected-today categories as
/// individual status rows, then a subheading and the roadmap categories packed
/// compactly (several per line) so a comprehensive catalog still fits a small
/// console. Only the categories tagged for this profile appear, so a ground
/// station is never shown a flight controller.
fn catalog_body(theme: &Theme, profile: catalog::Profile, sys: &hw::SysProbe) -> Vec<String> {
    let mut body = Vec::new();
    let mut planned: Vec<&'static str> = Vec::new();
    for cat in catalog::catalog_for(profile) {
        if cat.availability == catalog::Availability::Planned {
            planned.push(cat.label);
        } else {
            body.push(catalog_row(theme, cat, &catalog::detect(cat, sys)));
        }
    }
    if !planned.is_empty() {
        body.push(String::new());
        body.push(theme.dim("Also supported — plug it in and rescan:"));
        for line in pack_labels(&planned, 54) {
            body.push(format!("   {}", theme.dim(&line)));
        }
    }
    body
}

/// Pack a list of short labels into comma-joined lines no wider than `width`
/// display columns each (pure). Keeps the roadmap list a few lines rather than
/// one row per item.
fn pack_labels(labels: &[&str], width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    for &label in labels {
        let piece = if cur.is_empty() {
            label.to_string()
        } else {
            format!(", {label}")
        };
        if !cur.is_empty() && cur.chars().count() + piece.chars().count() > width {
            lines.push(std::mem::take(&mut cur));
            cur.push_str(label);
        } else {
            cur.push_str(&piece);
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

/// One catalog row: a status glyph, the fixed-width label, and a dim detail.
/// Found → a green tick + the path/id; a probed-but-absent Now category → a dim
/// dash + "not detected".
fn catalog_row(theme: &Theme, cat: &catalog::HwCategory, det: &catalog::Detection) -> String {
    let (mark, detail) = match det {
        catalog::Detection::Found(d) => (theme.ok(theme.glyph_ok()), format!("found · {d}")),
        catalog::Detection::Missing => (
            theme.dim(if theme.ascii { "-" } else { "—" }),
            "not detected".to_string(),
        ),
        // A Now row never carries Supported (that is the Planned-only state,
        // rendered compactly above), so nothing else reaches here.
        catalog::Detection::Supported => (theme.dim("·"), cat.note.to_string()),
    };
    format!(" {mark} {:<26}{}", cat.label, theme.dim(&detail))
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
    let is_drone = profile == "drone";
    let is_ground = profile == "ground_station";
    let is_workstation = profile == "workstation" || profile == "compute";

    // The feature list is profile-shaped: a drone / ground station is offered the
    // long-range radio (default on, but OPTIONAL — a Wi-Fi-indoor or a future
    // LoRa build can turn it off); a workstation has no long-range radio and no
    // onboard camera/display, so it is offered only the internet-reach option.
    // A workstation never installs the RTL driver; for drone/ground the radio
    // item's result below decides `no_rtl_driver`.
    args.no_rtl_driver = is_workstation;

    let mut items = Vec::new();
    if is_drone || is_ground {
        items.push(CheckItem {
            id: "radio".into(),
            label: "Long-range radio link".into(),
            benefit: "HD video and telemetry over an RTL8812EU WFB radio.".into(),
            checked: true,
            // Optional: a drone can fly on Wi-Fi indoors, or a future LoRa link,
            // so the operator can turn the RTL8812EU radio stack off.
            locked: false,
        });
    }
    if is_drone {
        items.push(CheckItem {
            id: "camera".into(),
            label: "Camera video".into(),
            benefit: "H.264/H.265 video from a USB or CSI camera.".into(),
            checked: hw.camera.is_some(),
            locked: false,
        });
    }
    if is_ground {
        items.push(CheckItem {
            id: "display".into(),
            label: "Status screen".into(),
            benefit: "Link + status on an attached I2C/SPI OLED or LCD.".into(),
            checked: hw.sys.i2c_addrs.contains(&0x3c) || hw.sys.i2c_addrs.contains(&0x3d),
            locked: false,
        });
    }
    items.push(CheckItem {
        id: "cloud".into(),
        label: "Reach it from anywhere".into(),
        benefit: if is_workstation {
            "Sign in to your account over the cloud relay.".into()
        } else {
            "Internet reach via the cloud relay (MQTT + WebRTC).".into()
        },
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
        vec![
            theme.dim("You're already on Wi-Fi, so this step is optional."),
            theme.dim("A network keeps the device reachable for indoor and"),
            theme.dim("bench testing, or off a phone hotspot in the field."),
            theme.dim("You can also set Wi-Fi up later from the dashboard."),
        ]
    } else {
        vec![
            theme.dim("Connect to Wi-Fi so the device stays reachable after"),
            theme.dim("you unplug the cable: indoor and bench testing, or"),
            theme.dim("off a phone hotspot in the field. Optional for now."),
        ]
    };
    // Skip is listed first; it is the default once the box is already on Wi-Fi
    // (the common case here), otherwise the card nudges toward setting it up so a
    // wired-only device is not stranded when the cable comes out.
    match confirm_card(
        tty,
        theme,
        WIFI,
        "Set up Wi-Fi now?",
        &detail,
        "Skip",
        "Set up Wi-Fi",
        on_wifi,
    ) {
        Flow::Value(true) => return Nav::Next,
        Flow::Value(false) => {}
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
        // Scan with an animated one-line live board.
        let scan_board = [BoardItem {
            label: "Looking for Wi-Fi networks…".into(),
            state: ItemState::Active,
            detail: None,
        }];
        let scan_iface = iface.clone();
        let nets = match widgets::run_with_spinner(tty, theme, WIFI, &scan_board, move || {
            wifi::scan(&scan_iface)
        }) {
            Spin::Done(n) => n,
            Spin::Aborted => return Nav::Abort,
        };

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
            // Require a non-empty key but do not impose the 8-char WPA2 floor —
            // WEP (5/13) and other short keys are valid, and nmcli validates the
            // real key on connect (a bad one lands on the retry path).
            match password_input(tty, theme, WIFI, &format!("Password for  {ssid}"), 1) {
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
    // Connect with an animated board.
    let c_iface = iface.to_string();
    let c_ssid = ssid.to_string();
    let c_pw = password.map(str::to_string);
    let connect_res: Result<(), String> =
        match widgets::run_with_spinner(tty, theme, WIFI, &items, move || {
            wifi::connect(&c_iface, &c_ssid, c_pw.as_deref(), hidden)
        }) {
            Spin::Done(r) => r,
            Spin::Aborted => return JoinOutcome::Abort,
        };
    if let Err(e) = connect_res {
        items[0].state = ItemState::Failed;
        items[0].detail = Some(short_reason(&e));
        paint_board(tty, theme, WIFI, &items, 0);
        return retry_prompt(tty, theme);
    }
    items[0].state = ItemState::Ok;
    items[0].detail = Some(format!("on {iface}"));
    items[1].state = ItemState::Active;

    // Verify LAN reachability with an animated board.
    let v_iface = iface.to_string();
    let reach = match widgets::run_with_spinner(tty, theme, WIFI, &items, move || {
        wifi::verify_lan_reachable(&v_iface)
    }) {
        Spin::Done(r) => r,
        Spin::Aborted => return JoinOutcome::Abort,
    };
    if !reach.reachable {
        items[1].state = ItemState::Failed;
        items[1].detail = Some("could not reach your network".into());
        paint_board(tty, theme, WIFI, &items, 0);
        // The join associated but the LAN did not answer; tear down the profile
        // nmcli just saved so a dead network is not left to auto-reconnect.
        wifi::forget(ssid);
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
    // Pre-fill from the device's real current hostname so the "reachable at
    // <host>.local" line reflects reality; fall back to a synthesized default
    // only when there is no usable hostname.
    let current = env::current_hostname();
    let seed = current.clone().unwrap_or_else(|| default_name(&profile));

    // When the box already has a real hostname, keeping it is the default: it's
    // reachable at that name right now and setup ran over it. Offer a quick
    // Keep, or Rename to pick a friendlier one.
    if let Some(host) = &current {
        match confirm_card(
            tty,
            theme,
            SETUP,
            "Name this device",
            &[
                theme.dim(&format!("It's reachable now at {host}.local.")),
                theme.dim("Keep this name, or pick a friendlier one."),
            ],
            &format!("Keep {host}"),
            "Rename",
            true,
        ) {
            Flow::Value(true) => {
                args.name = Some(host.clone());
                return Nav::Next;
            }
            Flow::Value(false) => {}
            Flow::Back => return Nav::Back,
            Flow::Abort => return Nav::Abort,
        }
    }

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
        // Later is listed first and is the default: pairing is completely
        // optional, since the device can be added by name or reached by its
        // URL/IP once setup finishes, and paired anytime with `ados pair`.
        match confirm_card(
            tty,
            theme,
            SETUP,
            "Connect to Mission Control now?",
            &[
                theme.dim("Optional. You can connect anytime later."),
                theme.dim("Add it in Mission Control by its name (no code needed),"),
                theme.dim("or just open its URL or IP once setup finishes."),
                theme.dim("To pair from the terminal later, run: ados pair"),
            ],
            "Later",
            "Enter a code",
            true,
        ) {
            Flow::Value(true) => return Nav::Next,
            Flow::Value(false) => match widgets::text_input(
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

fn review_stage(
    tty: &mut Tty,
    theme: &Theme,
    args: &mut Args,
    extras: &mut WizardExtras,
    collected: &Collected,
) -> ReviewNav {
    // The operating-region control only makes sense on a profile that transmits
    // (drone / ground station); a workstation has no long-range radio, so it is
    // omitted from the review actions.
    let has_radio = !matches!(
        args.profile.as_deref(),
        Some("workstation") | Some("compute")
    );
    loop {
        let summary = review_summary(theme, args, extras, collected);
        let mut choices = vec![
            Choice::new("finish", "Finish and set up", None),
            Choice::new("change", "Change an answer", None),
        ];
        if has_radio {
            choices.push(Choice::new("region", "Operating region (advanced)", None));
        }
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
            Flow::Value(1) => match change_answer(tty, theme, args) {
                Some(step) => return ReviewNav::JumpTo(step),
                None => continue,
            },
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
    let is_drone = profile == "drone";
    let is_ground = profile == "ground_station";
    let has_radio = is_drone || is_ground;
    let name = args
        .name
        .clone()
        .or_else(env::current_hostname)
        .unwrap_or_else(|| default_name(&profile));
    let mut rows = vec![kv(
        theme,
        "Device",
        &format!("{name}   ({})", friendly_profile(&profile)),
    )];
    // Radio / camera / display rows only for the profiles that have them.
    if has_radio {
        rows.push(kv(theme, "Radio", onoff(!args.no_rtl_driver)));
    }
    if is_drone {
        rows.push(kv(
            theme,
            "Camera",
            onoff(args.camera.as_deref() == Some("auto")),
        ));
    }
    if is_ground {
        rows.push(kv(
            theme,
            "Screen",
            onoff(args.display.as_deref() == Some("auto")),
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
    // Region + pairing are radio-profile concerns; a workstation shows neither.
    if has_radio {
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
    }
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
    fn hardware_frame_fits_and_is_context_aware_per_profile() {
        use crate::ui::theme::Theme;
        use crate::wizard::frame::{compose, Chrome, Screen, TermSize};

        let theme = Theme::detect(true, true); // no color, ASCII — readable + portable
        let sys = hw::SysProbe::default(); // nothing attached: every Now row reads "not detected"
        let size = TermSize { cols: 90, rows: 30 };
        for (profile, id, want, forbid) in [
            (
                catalog::Profile::Drone,
                "drone",
                "Flight controller",
                "HDMI output",
            ),
            (
                catalog::Profile::GroundStation,
                "ground_station",
                "Long-range radio",
                "Flight controller",
            ),
            (
                catalog::Profile::Workstation,
                "workstation",
                "This computer",
                "Long-range radio",
            ),
        ] {
            let body = catalog_body(&theme, profile, &sys);
            let screen = Screen {
                section: SETUP,
                body: &body,
                footer: "Enter to continue",
            };
            let chrome = Chrome {
                step: 2,
                total: 6,
                label: "Hardware".to_string(),
            };
            let grid = compose(&theme, &chrome, &screen, size);
            // Every row fits the terminal width exactly (no overflow off-screen).
            for (i, line) in grid.iter().enumerate() {
                assert_eq!(
                    line.chars().count(),
                    90,
                    "{id}: row {i} not 90 cols: {line:?}"
                );
            }
            let joined = grid.join("\n");
            // The whole body lands on screen (nothing clipped past the footer): the
            // last catalog label must be present.
            assert!(joined.contains(want), "{id}: missing expected {want:?}");
            // Context-awareness: the other profile's hardware must not appear.
            assert!(
                !joined.contains(forbid),
                "{id}: leaked another profile's {forbid:?}"
            );
            // Uncomment locally to eyeball the layout: `cargo test hardware_frame -- --nocapture`.
            // eprintln!("\n==== {id} ====\n{joined}");
        }
    }

    #[test]
    fn pack_labels_wraps_within_width_and_covers_all() {
        let labels = [
            "Camera gimbal",
            "Distance sensor",
            "Optical flow",
            "LiDAR",
            "mmWave radar",
            "RTK GNSS",
        ];
        let lines = pack_labels(&labels, 30);
        // Every line is within the width.
        for l in &lines {
            assert!(l.chars().count() <= 30, "line too wide: {l:?}");
        }
        // Every label survives, in order, across the joined lines.
        let joined = lines.join(", ");
        for l in labels {
            assert!(joined.contains(l), "lost label {l}");
        }
        assert!(
            lines.len() > 1,
            "should wrap into multiple lines at width 30"
        );
        // A single label longer than the width still emits (not dropped).
        assert_eq!(pack_labels(&["a-very-long-single-label"], 5).len(), 1);
        assert!(pack_labels(&[], 40).is_empty());
    }

    #[test]
    fn visible_steps_omit_pairing_for_a_workstation() {
        let drone = Args {
            profile: Some("drone".into()),
            ..Args::default()
        };
        assert!(
            visible_steps(&drone).contains(&Step::Pair),
            "a drone should walk the pairing step"
        );
        let gs = Args {
            profile: Some("ground_station".into()),
            ..Args::default()
        };
        assert!(visible_steps(&gs).contains(&Step::Pair));
        for p in ["workstation", "compute"] {
            let ws = Args {
                profile: Some(p.into()),
                ..Args::default()
            };
            let steps = visible_steps(&ws);
            assert!(
                !steps.contains(&Step::Pair),
                "{p} must not walk the pairing step"
            );
            // Review is always the terminal step.
            assert_eq!(steps.last(), Some(&Step::Review));
        }
    }

    #[test]
    fn friendly_profile_names() {
        assert_eq!(friendly_profile("drone"), "Drone");
        assert_eq!(friendly_profile("ground_station"), "Ground station");
        assert_eq!(friendly_profile("workstation"), "Workstation");
        assert_eq!(friendly_profile("anything-else"), "Drone");
    }
}
