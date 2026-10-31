//! Profile, hardware and component-catalog stages.

use ados_term::theme::Theme;
use ados_term::tty::Tty;
use ados_term::widgets::{checklist, select_list, CheckItem, Choice, Flow};

use crate::cli::Args;
use crate::steps::extensions::world_engine_default;
use crate::wizard::widgets::{ack_card, Ack, BoardItem, ItemState, Spin};
use crate::wizard::{catalog, hw, widgets, WizardExtras};

use super::{Nav, SETUP};

pub(super) fn profile_stage(tty: &mut Tty, theme: &Theme, args: &mut Args) -> Nav {
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
            set_profile(args, &choices[i].id);
            Nav::Next
        }
        Flow::Back => Nav::Back,
        Flow::Abort => Nav::Abort,
    }
}

/// Record the chosen profile. A CHANGE of profile drops the World Engine
/// answer so the features screen offers the new profile's default; the first
/// pick keeps a `--world-engine` / `--no-world-engine` the operator passed.
pub(super) fn set_profile(args: &mut Args, chosen: &str) {
    if args.profile.as_deref().is_some_and(|p| p != chosen) {
        args.world_engine = None;
    }
    args.profile = Some(chosen.to_string());
}

// ── stage: hardware check ──────────────────────────────────────────────────

pub(super) fn hardware_stage(
    tty: &mut Tty,
    theme: &Theme,
    args: &Args,
    hw: &mut hw::HardwareProbe,
) -> Nav {
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
pub(super) fn catalog_body(
    theme: &Theme,
    profile: catalog::Profile,
    sys: &hw::SysProbe,
) -> Vec<String> {
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
pub(super) fn pack_labels(labels: &[&str], width: usize) -> Vec<String> {
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

pub(super) fn components_stage(
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
    // onboard camera/display, so it is offered the internet-reach option. Every
    // profile is offered the World Engine extension, on by default where its
    // heavy half runs (workstation / compute). A workstation never installs the
    // RTL driver; for drone/ground the radio item's result below decides
    // `no_rtl_driver`.
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
    items.push(world_engine_item(args, &profile));

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
                    WORLD_ENGINE_ITEM => args.world_engine = Some(it.checked),
                    _ => {}
                }
            }
            Nav::Next
        }
        Flow::Back => Nav::Back,
        Flow::Abort => Nav::Abort,
    }
}

/// The checklist id of the World Engine feature row.
const WORLD_ENGINE_ITEM: &str = "world_engine";

/// The World Engine feature row: pre-set to the operator's answer so far (a
/// flag, or an earlier pass through this screen), else the profile default.
pub(super) fn world_engine_item(args: &Args, profile: &str) -> CheckItem {
    CheckItem {
        id: WORLD_ENGINE_ITEM.into(),
        label: "World Engine (3D world model + compute offload)".into(),
        benefit: "3D capture and reconstruction, plus perception offload.".into(),
        checked: args
            .world_engine
            .unwrap_or_else(|| world_engine_default(profile)),
        locked: false,
    }
}

// ── stage: Wi-Fi ───────────────────────────────────────────────────────────
