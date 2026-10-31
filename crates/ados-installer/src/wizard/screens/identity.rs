//! Node name and pairing-code stages.

use ados_term::theme::Theme;
use ados_term::tty::Tty;
use ados_term::widgets::{confirm_card, text_input, Flow};

use crate::cli::Args;
use crate::env;
use crate::steps::config_identity::slugify_hostname;
use crate::wizard::widgets::{insert_hostname_char, insert_pair_char};

use super::{Nav, SETUP};

pub(super) fn name_stage(tty: &mut Tty, theme: &Theme, args: &mut Args) -> Nav {
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
        // Show the LAN IP alongside the .local name: the IP works everywhere
        // (including the hosted GCS), while the .local name needs a desktop app.
        let ip = env::lan_ips().into_iter().next();
        let reach = match &ip {
            Some(ip) => format!("It's reachable now at {host}.local or {ip}."),
            None => format!("It's reachable now at {host}.local."),
        };
        let mut body: Vec<String> = vec![theme.dim(&reach)];
        if ip.is_some() {
            body.push(theme.dim("Direct IP is most reliable; .local needs a desktop app."));
        }
        body.push(theme.dim("Keep this name, or pick a friendlier one."));
        match confirm_card(
            tty,
            theme,
            SETUP,
            "Name this device",
            &body,
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

    match text_input(
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
pub(super) fn default_name(profile: &str) -> String {
    let short = match profile {
        "ground_station" => "ground",
        "workstation" => "workstation",
        "compute" => "compute",
        _ => "drone",
    };
    format!("ados-{short}-01")
}

// ── stage: pairing ─────────────────────────────────────────────────────────

pub(super) fn pair_stage(tty: &mut Tty, theme: &Theme, args: &mut Args) -> Nav {
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
            Flow::Value(false) => match text_input(
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
