//! Settings page: SYSTEM & SAFETY › Region. The RF posture: `unrestricted`
//! (the default) or a pinned ISO 3166-1 alpha-2 region, read from the
//! effective config (`network.regulatory.mode` / `.region`). Staged on the
//! page, applied after a confirm via `POST /api/v1/setup/apply {regulatory:
//! {mode, region?}}`.

use ratatui::layout::Rect;
use ratatui::Frame;
use serde_json::{json, Value};

use super::kit;
use crate::app::{Core, FrameOut, Hit};
use crate::keymap::Command;
use crate::overlay::{Overlay, OverlayResult, Ret};
use crate::poll::{ResourceId, Sub};
use crate::settings::apply_result;
use crate::settings::page::{Bespoke, BespokeId};
use crate::settings::rows_view::{self, Item, RowsState};
use crate::settings::schema::Kind;
use crate::toast::ToastKind;
use crate::worker::{Purpose, Reply, Request, Route};

/// Common operating regions; any other ISO code is typed in.
pub const COMMON_REGIONS: &[(&str, &str)] = &[
    ("US", "United States"),
    ("IN", "India"),
    ("DE", "Germany"),
    ("GB", "United Kingdom"),
    ("AU", "Australia"),
    ("JP", "Japan"),
    ("CA", "Canada"),
];
const OTHER: &str = "Other ISO code";
const MODES: &[&str] = &["unrestricted", "region"];

const MODE_HELP: &str = "Unrestricted: the radio works anywhere with no region enforced (operator responsible for local RF compliance). Region: enforce one jurisdiction's channel and power limits.";

fn route() -> Route {
    Route::page(BespokeId::Region.id())
}

/// An uppercase two-letter code, or `None`.
pub fn normalize_region(raw: &str) -> Option<String> {
    let code = raw.trim().to_ascii_uppercase();
    (code.len() == 2 && code.bytes().all(|b| b.is_ascii_uppercase())).then_some(code)
}

/// `United States (US)`, or the bare code for an uncommon one.
pub fn region_label(code: &str) -> String {
    match COMMON_REGIONS.iter().find(|(c, _)| *c == code) {
        Some((c, label)) => format!("{label} ({c})"),
        None => code.to_string(),
    }
}

fn validate_iso(s: &str) -> Result<(), String> {
    normalize_region(s)
        .map(|_| ())
        .ok_or_else(|| "Enter a valid two-letter country code.".into())
}

#[derive(Default)]
pub struct Page {
    rows: RowsState,
    staged_mode: Option<String>,
    staged_region: Option<String>,
    busy: bool,
}

/// The agent's mode and pinned region, from the effective config the
/// regulatory apply persists to.
fn current(core: &Core) -> (String, Option<String>) {
    let c = core.value(&ResourceId::Config);
    let mode = if kit::text(c, "network.regulatory.mode") == Some("region") {
        "region"
    } else {
        "unrestricted"
    };
    let region = if mode == "region" {
        kit::text(c, "network.regulatory.region").and_then(normalize_region)
    } else {
        None
    };
    (mode.to_string(), region)
}

impl Page {
    fn effective(&self, core: &Core) -> (String, Option<String>) {
        let (m, r) = current(core);
        (
            self.staged_mode.clone().unwrap_or(m),
            self.staged_region.clone().or(r),
        )
    }

    fn dirty(&self, core: &Core) -> bool {
        let (cm, cr) = current(core);
        let (m, r) = self.effective(core);
        m != cm || (m == "region" && r != cr)
    }

    fn items(&self, core: &Core) -> Vec<Item> {
        let (cm, cr) = current(core);
        let (m, r) = self.effective(core);
        let mut items = vec![Item::Header("Current".into())];
        if let Some(note) = kit::resource_note(core, &ResourceId::Config, "the config") {
            items.push(note);
        }
        let now = match (&cm[..], &cr) {
            ("region", Some(code)) => format!("region · enforcing {}", region_label(code)),
            _ => "unrestricted".into(),
        };
        items.push(rows_view::info(core, "Posture", now));
        items.push(Item::Header("Operating region".into()));
        let mut row = kit::row("mode", "Operating region", MODE_HELP, kit::enum_of(MODES), Some(json!(m)));
        row.default = Some(json!(cm));
        items.push(Item::Row(row));
        if m == "region" {
            let value = r.as_deref().map(region_label).unwrap_or_else(|| "pick a region".into());
            let mut row = kit::row(
                "region",
                "Region",
                "The ISO 3166-1 alpha-2 region whose channel and power rules the radio enforces",
                Kind::Str { pattern: None },
                Some(json!(value)),
            );
            row.default = Some(json!(cr.as_deref().map(region_label).unwrap_or_default()));
            items.push(Item::Row(row));
        } else {
            items.push(rows_view::note(
                core,
                "Unrestricted. Operator responsible for local RF compliance.",
                true,
            ));
        }
        let detail = if self.busy {
            format!("{} Applying…", core.sym.spinner_frame(core.frame as usize))
        } else if self.dirty(core) {
            "unsaved changes; the radio re-reads this at the next restart".into()
        } else {
            "no changes".into()
        };
        items.push(kit::action(core, "apply", "Apply", &detail));
        items
    }

    fn cycle(&mut self, core: &Core, key: &str, forward: bool) {
        match key {
            "mode" => {
                let (m, _) = self.effective(core);
                let next = if m == "region" { "unrestricted" } else { "region" };
                self.staged_mode = Some(next.to_string());
            }
            "region" => {
                let (_, r) = self.effective(core);
                let n = COMMON_REGIONS.len();
                let pos = r.as_deref().and_then(|c| COMMON_REGIONS.iter().position(|(x, _)| *x == c));
                let next = match (pos, forward) {
                    (Some(i), true) => (i + 1) % n,
                    (Some(i), false) => (i + n - 1) % n,
                    (None, _) => 0,
                };
                self.staged_region = Some(COMMON_REGIONS[next].0.to_string());
            }
            _ => {}
        }
    }

    fn pick(&self, core: &mut Core) {
        let (_, r) = self.effective(core);
        let mut items: Vec<String> = COMMON_REGIONS.iter().map(|(c, _)| region_label(c)).collect();
        items.push(OTHER.into());
        let sel = match r.as_deref() {
            Some(c) => COMMON_REGIONS.iter().position(|(x, _)| *x == c).unwrap_or(COMMON_REGIONS.len()),
            None => 0,
        };
        core.open(Overlay::picker(Ret::new(route(), "pick"), "Region", items, sel));
    }

    fn apply(&mut self, core: &mut Core) {
        if !self.dirty(core) {
            core.toast(ToastKind::Info, "No region change to apply");
            return;
        }
        let (m, r) = self.effective(core);
        if m == "region" && r.is_none() {
            core.toast(ToastKind::Error, "Pick a region first");
            return;
        }
        if !core.guard_disarmed() {
            return;
        }
        let first = match (&m[..], &r) {
            ("region", Some(code)) => format!("The radio will enforce {} channel and power rules.", region_label(code)),
            _ => "The radio will run unrestricted, with no region enforced. You are responsible for legal RF operation in your jurisdiction.".into(),
        };
        let section = json!({ "mode": m, "region": if m == "region" { json!(r) } else { Value::Null } });
        core.open(Overlay::confirm(
            Ret::new(route(), "apply").with(section),
            "Change operating region?",
            vec![first, "The radio re-reads this at the next restart.".into()],
        ));
    }
}

impl Bespoke for Page {
    fn subs(&self, _core: &Core) -> Vec<Sub> {
        vec![Sub::every_ms(
            ResourceId::Config,
            "/api/config",
            crate::settings::config_rows::CONFIG_EVERY_MS,
        )]
    }

    fn on_command(&mut self, core: &mut Core, cmd: Command) -> bool {
        let items = self.items(core);
        if kit::nav(&mut self.rows, &items, cmd) {
            return true;
        }
        let key = self.rows.focused_key(&items).map(str::to_string);
        match (cmd, key.as_deref()) {
            (Command::Left | Command::Right, Some(k)) => self.cycle(core, k, cmd == Command::Right),
            (Command::Activate, Some("apply")) => self.apply(core),
            (Command::Activate, Some("region")) => self.pick(core),
            (Command::Activate, Some("mode")) => self.cycle(core, "mode", true),
            (Command::Left | Command::Right | Command::Activate, None) => {}
            _ => return false,
        }
        true
    }

    fn on_overlay(&mut self, core: &mut Core, ret: Ret, result: OverlayResult) {
        match (ret.tag, result) {
            ("pick", OverlayResult::Picked(i)) => match COMMON_REGIONS.get(i) {
                Some((code, _)) => self.staged_region = Some(code.to_string()),
                None => {
                    let (_, r) = self.effective(core);
                    let initial = r.filter(|c| !COMMON_REGIONS.iter().any(|(x, _)| x == c)).unwrap_or_default();
                    let mut s = Overlay::input(Ret::new(route(), "iso"), "Region", "ISO 3166-1 alpha-2 code", &initial);
                    s.validate = Some(validate_iso);
                    s.hint = Some("e.g. FR".into());
                    core.open(Overlay::Input(s));
                }
            },
            ("iso", OverlayResult::Text(t)) => self.staged_region = normalize_region(&t),
            ("apply", OverlayResult::Confirmed) => {
                if !core.guard_disarmed() {
                    return;
                }
                self.busy = true;
                core.submit(
                    Request::new("POST", "/api/v1/setup/apply", route(), Purpose::SetupApply { section: "regulatory" })
                        .body(json!({ "regulatory": ret.data })),
                );
            }
            _ => {}
        }
    }

    fn on_reply(&mut self, core: &mut Core, reply: Reply) {
        if !matches!(reply.purpose, Purpose::SetupApply { .. }) {
            return;
        }
        self.busy = false;
        if let Some(body) = apply_result::show(core, "Operating region", &reply.result) {
            let ok = apply_result::succeeded(body, "regulatory");
            if ok {
                self.staged_mode = None;
                self.staged_region = None;
            }
            let restart = apply_result::section(body, "regulatory")
                .and_then(|s| crate::app::at(s, "data.restart_required"))
                .and_then(Value::as_bool)
                == Some(true);
            if ok && restart {
                core.toast(ToastKind::Warn, "Radio restart required");
            }
            core.flash("region:mode", ok);
        }
        core.fetch_now(ResourceId::Config);
    }

    fn on_click(&mut self, core: &mut Core, hit: &Hit) {
        let items = self.items(core);
        kit::click(&mut self.rows, &items, hit);
    }

    fn render(&mut self, f: &mut Frame, area: Rect, core: &Core, out: &mut FrameOut) {
        let items = self.items(core);
        rows_view::render(f, area, core, &items, &mut self.rows, "region:", true, out);
    }

    fn context_line(&self, core: &Core) -> Option<String> {
        let items = self.items(core);
        match self.rows.focused_key(&items)? {
            "apply" => Some("Apply the staged posture after a confirm".into()),
            "region" => Some("←→ cycles common regions · Enter picks, or types another ISO code".into()),
            _ => self.rows.focused_row(&items).map(|r| r.help.clone()),
        }
    }

    fn animating(&self, _core: &Core) -> bool {
        self.busy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_codes_normalize_and_label() {
        assert_eq!(normalize_region(" fr "), Some("FR".into()));
        assert_eq!(normalize_region("FRA"), None);
        assert_eq!(normalize_region("1A"), None);
        assert_eq!(region_label("IN"), "India (IN)");
        assert_eq!(region_label("FR"), "FR");
    }

    #[test]
    fn the_posture_reads_from_config_and_unrestricted_can_be_staged() {
        let mut app = crate::ui::tests::test_app();
        let core = &mut app.core;
        kit::put(
            core,
            ResourceId::Config,
            json!({"network": {"regulatory": {"mode": "region", "region": "US", "ack_operator": "bench"}}}),
        );
        // The setup status never carries the posture; it must not matter.
        kit::put(core, ResourceId::SetupStatus, json!({"regulatory": {}}));
        let mut page = Page::default();
        assert_eq!(current(core), ("region".to_string(), Some("US".to_string())));
        let items = page.items(core);
        let posture = items.iter().find_map(|i| match i {
            Item::Row(r) if r.key == "region" => r.value.clone(),
            _ => None,
        });
        assert_eq!(posture, Some(json!("United States (US)")));
        assert!(!page.dirty(core));
        page.rows.focus_key(&items, "mode");
        assert!(page.on_command(core, Command::Right));
        assert_eq!(page.effective(core).0, "unrestricted");
        assert!(page.dirty(core), "unrestricted is staged against the pinned US");
    }
}
