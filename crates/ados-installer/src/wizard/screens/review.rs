//! The review stage and its summary rendering.

use ados_term::theme::Theme;
use ados_term::tty::Tty;
use ados_term::widgets::{select_list, summary_select, text_input, Choice, Flow};

use crate::cli::Args;
use crate::env;
use crate::steps::extensions::world_engine_default;
use crate::wizard::widgets::insert_region_char;
use crate::wizard::WizardExtras;

use super::identity::default_name;
use super::{change_answer, Collected, ReviewNav};

pub(super) fn review_stage(
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
pub(super) fn review_summary(
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
    rows.push(kv(
        theme,
        "Extension",
        if args
            .world_engine
            .unwrap_or_else(|| world_engine_default(&profile))
        {
            "World Engine"
        } else {
            "none"
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
pub(super) fn friendly_profile(profile: &str) -> &'static str {
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
                "__other" => match text_input(
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
