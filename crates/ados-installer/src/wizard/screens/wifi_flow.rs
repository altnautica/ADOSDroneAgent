//! The Wi-Fi stage: network pick, hidden SSID and the join/retry flow.

use ados_term::theme::Theme;
use ados_term::tty::Tty;
use ados_term::widgets::{confirm_card, password_input, text_input, Flow};

use crate::wizard::widgets::{
    ack_card, insert_ssid_char, paint_board, Ack, BoardItem, ItemState, Spin, WifiPick, WifiRow,
};
use crate::wizard::{widgets, wifi};

use super::{Collected, JoinOutcome, Nav, WIFI};

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
