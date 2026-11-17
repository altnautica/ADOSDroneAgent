//! The Wi-Fi stage: network pick, hidden SSID, the join/retry flow and an
//! optional backup network.

use ados_term::progress::ProgressSink;
use ados_term::theme::Theme;
use ados_term::tty::Tty;
use ados_term::widgets::{confirm_card, password_input, text_input, Flow};

use crate::net_stack;
use crate::wizard::widgets::{
    ack_card, insert_ssid_char, paint_board, Ack, BoardItem, ItemState, Spin, WifiPick, WifiRow,
};
use crate::wizard::wifi::WifiRole;
use crate::wizard::{widgets, wifi};

use super::{Collected, JoinInfo, JoinOutcome, Nav, WIFI};

pub(super) fn wifi_stage(tty: &mut Tty, theme: &Theme, collected: &mut Collected) -> Nav {
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

    if let Some(nav) = adopt_network_stack(tty, theme) {
        return nav;
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

    let (primary, info) = match pick_and_join(tty, theme, &iface, None) {
        Picked::Joined(ssid, info) => (ssid, info),
        Picked::Skip => return Nav::Next,
        Picked::Back => return Nav::Back,
        Picked::Abort => return Nav::Abort,
    };
    collected.wifi_ssid = Some(primary.clone());

    match confirm_card(
        tty,
        theme,
        WIFI,
        "Add a backup network?",
        &[theme.dim("If the main network is out of range, the device joins this one instead.")],
        "Not now",
        "Add backup",
        true,
    ) {
        Flow::Value(false) => match pick_and_join(tty, theme, &iface, Some(&primary)) {
            Picked::Joined(ssid, _) => collected.wifi_backup_ssid = Some(ssid),
            Picked::Abort => return Nav::Abort,
            Picked::Skip | Picked::Back => {}
        },
        Flow::Abort => return Nav::Abort,
        Flow::Value(true) | Flow::Back => {}
    }

    let mut lines = vec![theme.ok(&format!("{} Main network: {primary}", theme.glyph_ok()))];
    if let Some(b) = &collected.wifi_backup_ssid {
        lines.push(theme.ok(&format!("{} Backup network: {b}", theme.glyph_ok())));
    }
    match (wifi::iface_ipv4(&iface), &info.gateway) {
        (Some(ip), _) => lines.push(theme.dim(&format!("This device on Wi-Fi: {ip}"))),
        (None, Some(g)) => lines.push(theme.dim(&format!("Reached your network (gateway {g})."))),
        (None, None) => {}
    }
    lines.push(theme.bold("You can unplug the network cable now."));
    match ack_card(tty, theme, WIFI, "Wi-Fi is set up", &lines, false) {
        Flow::Abort => Nav::Abort,
        _ => Nav::Next,
    }
}

/// Hand the management Wi-Fi to NetworkManager when this host runs another
/// network stack. `None` means carry on with the Wi-Fi setup; `Some` ends the
/// stage (the adoption failed and the operator has been told).
fn adopt_network_stack(tty: &mut Tty, theme: &Theme) -> Option<Nav> {
    let stack = net_stack::probe();
    // No Wi-Fi radio: the stage says so next, nothing to adopt it for.
    if stack.wifi_iface.is_none() || net_stack::plan(&stack).noop() {
        return None;
    }
    let reason = if net_stack::wifi_uplink_in_use() {
        // Taking the radio over would cut the link this device is using.
        "This device is online over Wi-Fi right now.".to_string()
    } else {
        let mut items = [BoardItem {
            label: "Getting Wi-Fi ready…".into(),
            state: ItemState::Active,
            detail: Some(format!("this system uses {}", stack.label())),
        }];
        let res = match widgets::run_with_spinner(tty, theme, WIFI, &items, || {
            net_stack::ensure_networkmanager(&ProgressSink::default())
        }) {
            Spin::Done(r) => r,
            Spin::Aborted => return Some(Nav::Abort),
        };
        match res {
            Ok(a) => {
                items[0].state = ItemState::Ok;
                items[0].detail = Some(if a.reboot_reason.is_some() {
                    // The wired link moves over at the install's closing restart.
                    "your wired link is unchanged until the next restart".into()
                } else {
                    "your wired link stays as it is".into()
                });
                paint_board(tty, theme, WIFI, &items, 0);
                return None;
            }
            Err(e) => short_reason(&e),
        }
    };
    Some(
        match ack_card(
            tty,
            theme,
            WIFI,
            "Wi-Fi setup isn't available here",
            &[
                theme.dim(&reason),
                theme.dim("Your wired link is unchanged."),
                theme.dim("You can set up Wi-Fi later from the dashboard."),
            ],
            false,
        ) {
            Flow::Value(_) => Nav::Next,
            Flow::Back => Nav::Back,
            Flow::Abort => Nav::Abort,
        },
    )
}

/// The result of picking and joining one network.
enum Picked {
    Joined(String, JoinInfo),
    Skip,
    Back,
    Abort,
}

/// Scan, pick, ask for the password and join, retrying on failure. With
/// `primary` set this is the backup network: the primary is left out of the
/// list, and the device switches back to it whatever happens.
fn pick_and_join(tty: &mut Tty, theme: &Theme, iface: &str, primary: Option<&str>) -> Picked {
    let role = if primary.is_some() {
        WifiRole::Backup
    } else {
        WifiRole::Primary
    };
    let mut attempted = false;
    let leave = |attempted: bool, p: Picked| {
        // A failed backup attempt can leave the radio on nothing.
        if attempted && !matches!(p, Picked::Joined(..)) {
            if let Some(main) = primary {
                let _ = wifi::activate(main);
            }
        }
        p
    };
    loop {
        // Scan with an animated one-line live board.
        let scan_board = [BoardItem {
            label: "Looking for Wi-Fi networks…".into(),
            state: ItemState::Active,
            detail: None,
        }];
        let scan_iface = iface.to_string();
        let nets = match widgets::run_with_spinner(tty, theme, WIFI, &scan_board, move || {
            wifi::scan(&scan_iface)
        }) {
            Spin::Done(n) => n,
            Spin::Aborted => return Picked::Abort,
        };

        let rows: Vec<WifiRow> = nets
            .iter()
            .filter(|n| Some(n.ssid.as_str()) != primary)
            .map(|n| WifiRow {
                ssid: n.ssid.clone(),
                signal: n.signal,
                secured: n.secured,
                in_use: n.in_use,
            })
            .collect();

        if rows.is_empty() {
            match ack_card(
                tty,
                theme,
                WIFI,
                "No networks found",
                &[theme.dim("No Wi-Fi networks were seen nearby.")],
                true,
            ) {
                Flow::Value(Ack::Rescan) => continue,
                Flow::Value(Ack::Continue) => return leave(attempted, Picked::Skip),
                Flow::Back => return leave(attempted, Picked::Back),
                Flow::Abort => return Picked::Abort,
            }
        }

        let (ssid, secured, hidden) = match widgets::wifi_picker(tty, theme, &rows) {
            Flow::Value(WifiPick::Rescan) => continue,
            Flow::Value(WifiPick::Network { ssid, secured }) => (ssid, secured, false),
            Flow::Value(WifiPick::Hidden) => match hidden_ssid(tty, theme) {
                Flow::Value(s) => (s, true, true),
                Flow::Back => continue,
                Flow::Abort => return Picked::Abort,
            },
            Flow::Back => return leave(attempted, Picked::Back),
            Flow::Abort => return Picked::Abort,
        };

        let password = if secured {
            // Require a non-empty key but do not impose the 8-char WPA2 floor —
            // WEP (5/13) and other short keys are valid, and nmcli validates the
            // real key on connect (a bad one lands on the retry path).
            match password_input(tty, theme, WIFI, &format!("Password for  {ssid}"), 1) {
                Flow::Value(p) => Some(p),
                Flow::Back => continue,
                Flow::Abort => return Picked::Abort,
            }
        } else {
            None
        };

        attempted = true;
        let target = JoinTarget {
            iface,
            ssid: &ssid,
            password: password.as_deref(),
            hidden,
            role,
            primary,
        };
        match join_flow(tty, theme, &target) {
            JoinOutcome::Connected(info) => return Picked::Joined(ssid, info),
            JoinOutcome::Retry => continue,
            JoinOutcome::Skip => return leave(attempted, Picked::Skip),
            JoinOutcome::Abort => return Picked::Abort,
        }
    }
}

/// Prompt for a hidden network's name.
fn hidden_ssid(tty: &mut Tty, theme: &Theme) -> Flow<String> {
    text_input(
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

/// One join attempt's parameters.
struct JoinTarget<'a> {
    iface: &'a str,
    ssid: &'a str,
    password: Option<&'a str>,
    hidden: bool,
    role: WifiRole,
    /// The main network to switch back to after saving a backup.
    primary: Option<&'a str>,
}

/// Connect → verify LAN reachability → persist (→ switch back to the main
/// network, for a backup), with a live board.
fn join_flow(tty: &mut Tty, theme: &Theme, t: &JoinTarget) -> JoinOutcome {
    let iface = t.iface;
    let ssid = t.ssid;
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
    if let Some(main) = t.primary {
        items.push(BoardItem {
            label: format!("Switching back to {main}"),
            state: ItemState::Queued,
            detail: None,
        });
    }
    // Connect with an animated board.
    let c_iface = iface.to_string();
    let c_ssid = ssid.to_string();
    let c_pw = t.password.map(str::to_string);
    let hidden = t.hidden;
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
        return retry_prompt(tty, theme, t.role);
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
        return retry_prompt(tty, theme, t.role);
    }
    items[1].state = ItemState::Ok;
    items[1].detail = reach.gateway.as_ref().map(|g| format!("gateway {g}"));
    items[2].state = ItemState::Active;
    paint_board(tty, theme, WIFI, &items, 0);

    wifi::persist(ssid, t.role);
    items[2].state = ItemState::Ok;
    paint_board(tty, theme, WIFI, &items, 0);

    if let Some(main) = t.primary {
        items[3].state = ItemState::Active;
        let (s_main, s_iface) = (main.to_string(), iface.to_string());
        let back = match widgets::run_with_spinner(tty, theme, WIFI, &items, move || {
            wifi::activate(&s_main).map(|()| wifi::verify_lan_reachable(&s_iface))
        }) {
            Spin::Done(r) => r,
            Spin::Aborted => return JoinOutcome::Abort,
        };
        // The backup is saved either way: NetworkManager rejoins the main
        // network by priority when it is in range.
        match back {
            Ok(r) if r.reachable => items[3].state = ItemState::Ok,
            Ok(_) => {
                items[3].state = ItemState::Failed;
                items[3].detail = Some("could not reach your network".into());
            }
            Err(e) => {
                items[3].state = ItemState::Failed;
                items[3].detail = Some(short_reason(&e));
            }
        }
        paint_board(tty, theme, WIFI, &items, 0);
    }

    JoinOutcome::Connected(JoinInfo {
        gateway: reach.gateway,
    })
}

/// Offer another network after a failed join.
fn retry_prompt(tty: &mut Tty, theme: &Theme, role: WifiRole) -> JoinOutcome {
    let (question, skip) = match role {
        WifiRole::Primary => ("That didn't connect. Try another network?", "Skip Wi-Fi"),
        WifiRole::Backup => ("That didn't connect. Try another backup?", "Skip backup"),
    };
    match confirm_card(tty, theme, WIFI, question, &[], "Try again", skip, true) {
        Flow::Value(true) => JoinOutcome::Retry,
        Flow::Value(false) => JoinOutcome::Skip,
        Flow::Back => JoinOutcome::Retry,
        Flow::Abort => JoinOutcome::Abort,
    }
}

/// Shorten an nmcli error to a single tidy line for the board detail.
pub(super) fn short_reason(reason: &str) -> String {
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
