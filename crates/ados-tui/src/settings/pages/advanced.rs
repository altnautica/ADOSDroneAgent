//! Settings page: SYSTEM & SAFETY › Advanced. The log level and the board
//! override, both written through `POST /api/v1/setup/apply {advanced:
//! {log_level?|board_override?}}`. The override slug is validated here
//! (`[A-Za-z0-9_-]{0,64}`) before anything is sent.

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
use crate::settings::row::cycle;
use crate::settings::rows_view::{self, Item, RowsState};
use crate::settings::schema::Kind;
use crate::toast::ToastKind;
use crate::worker::{Purpose, Reply, Request, Route};

const LEVELS: &[&str] = &["debug", "info", "warning", "error", "critical"];
const AUTO: &str = "auto-detect";

const LEVEL_HELP: &str = "Verbosity for the structured logger. Saved on selection; applies on the next service restart.";
const BOARD_HELP: &str = "Force the HAL detector to use a specific board profile slug. Leave blank for auto-detect. Wrong values can disable peripherals.";

fn route() -> Route {
    Route::page(BespokeId::Advanced.id())
}

/// A board-override slug: letters, digits, dash and underscore, at most 64.
pub fn validate_board_override(s: &str) -> Result<(), String> {
    if s.chars().count() > 64 {
        return Err("At most 64 characters.".into());
    }
    if !s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err("Letters, digits, dash and underscore only.".into());
    }
    Ok(())
}

#[derive(Default)]
pub struct Page {
    rows: RowsState,
    /// A log level sent and not yet answered (shown in place).
    pending_level: Option<String>,
    busy: bool,
}

fn config_str<'a>(core: &'a Core, path: &str) -> Option<&'a str> {
    core.value(&ResourceId::Config)
        .and_then(|c| crate::app::at(c, path))
        .and_then(Value::as_str)
}

impl Page {
    fn level(&self, core: &Core) -> String {
        self.pending_level
            .clone()
            .unwrap_or_else(|| config_str(core, "logging.level").unwrap_or("info").to_ascii_lowercase())
    }

    fn items(&self, core: &Core) -> Vec<Item> {
        let mut items = vec![Item::Header("Logging".into())];
        if let Some(note) = kit::resource_note(core, &ResourceId::Config, "the config") {
            items.push(note);
        }
        let mut row = kit::row("log_level", "Log level", LEVEL_HELP, kit::enum_of(LEVELS), Some(json!(self.level(core))));
        row.default = Some(json!("info"));
        items.push(Item::Row(row));
        items.push(Item::Header("Board".into()));
        let board = config_str(core, "agent.board_override").unwrap_or_default();
        let shown = if board.is_empty() { AUTO } else { board };
        let mut row = kit::row(
            "board_override",
            "Board override",
            BOARD_HELP,
            Kind::Str {
                pattern: Some("[A-Za-z0-9_-]{0,64}".into()),
            },
            Some(json!(shown)),
        );
        row.default = Some(json!(AUTO));
        items.push(Item::Row(row));
        if self.busy {
            items.push(rows_view::note(
                core,
                format!("{} Applying…", core.sym.spinner_frame(core.frame as usize)),
                false,
            ));
        }
        items
    }

    fn send_level(&mut self, core: &mut Core, level: String) {
        self.pending_level = Some(level.clone());
        core.submit(
            Request::new(
                "POST",
                "/api/v1/setup/apply",
                route(),
                Purpose::Page {
                    tag: "log_level",
                    data: json!(level),
                },
            )
            .body(json!({ "advanced": { "log_level": level } })),
        );
    }

    fn edit_board(&self, core: &mut Core) {
        let current = config_str(core, "agent.board_override").unwrap_or_default().to_string();
        let mut s = Overlay::input(Ret::new(route(), "board"), "Board override", "Slug (empty = auto-detect)", &current);
        s.validate = Some(validate_board_override);
        s.hint = Some("Letters, digits, dash and underscore, at most 64".into());
        core.open(Overlay::Input(s));
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
        let row = self.rows.focused_row(&items).cloned();
        match (cmd, row) {
            (Command::Left | Command::Right, Some(r)) if r.key == "log_level" => {
                if self.pending_level.is_some() {
                    return true;
                }
                if let Some(Value::String(next)) = cycle(&r.kind, r.value.as_ref(), cmd == Command::Right) {
                    self.send_level(core, next);
                }
            }
            (Command::Activate, Some(r)) if r.key == "log_level" => {
                let sel = LEVELS.iter().position(|l| *l == self.level(core)).unwrap_or(1);
                let items = LEVELS.iter().map(|l| l.to_string()).collect();
                core.open(Overlay::picker(Ret::new(route(), "level"), "Log level", items, sel));
            }
            (Command::Activate, Some(r)) if r.key == "board_override" => self.edit_board(core),
            (Command::Left | Command::Right | Command::Activate, _) => {}
            _ => return false,
        }
        true
    }

    fn on_overlay(&mut self, core: &mut Core, ret: Ret, result: OverlayResult) {
        match (ret.tag, result) {
            ("level", OverlayResult::Picked(i)) => {
                if let Some(level) = LEVELS.get(i) {
                    if Some(*level) != Some(self.level(core).as_str()) {
                        self.send_level(core, level.to_string());
                    }
                }
            }
            ("board", OverlayResult::Text(slug)) => {
                let slug = slug.trim().to_string();
                if let Err(e) = validate_board_override(&slug) {
                    core.toast(ToastKind::Error, e);
                    return;
                }
                let shown = if slug.is_empty() { AUTO.to_string() } else { slug.clone() };
                core.open(Overlay::confirm(
                    Ret::new(route(), "board_apply").with(json!(slug)),
                    "Override the detected board?",
                    vec![
                        format!("HAL will use {shown} on the next config reload."),
                        "If the slug is wrong, peripherals keyed on the board profile (display, GPIO, encoder API) may stop working until you clear the override.".into(),
                    ],
                ));
            }
            ("board_apply", OverlayResult::Confirmed) => {
                self.busy = true;
                core.submit(
                    Request::new(
                        "POST",
                        "/api/v1/setup/apply",
                        route(),
                        Purpose::Page {
                            tag: "board_override",
                            data: Value::Null,
                        },
                    )
                    .body(json!({ "advanced": { "board_override": ret.data } })),
                );
            }
            _ => {}
        }
    }

    fn on_reply(&mut self, core: &mut Core, reply: Reply) {
        let Purpose::Page { tag, data } = &reply.purpose else { return };
        match *tag {
            "log_level" => {
                self.pending_level = None;
                match &reply.result {
                    Ok(body) if apply_result::succeeded(body, "advanced") => {
                        let level = data.as_str().unwrap_or_default();
                        core.toast(ToastKind::Success, format!("Log level set to {level}."));
                        core.flash("advanced:log_level", true);
                    }
                    other => {
                        apply_result::show(core, "Log level", other);
                        core.flash("advanced:log_level", false);
                    }
                }
            }
            "board_override" => {
                self.busy = false;
                if let Some(body) = apply_result::show(core, "Board override", &reply.result) {
                    core.flash("advanced:board_override", apply_result::succeeded(body, "advanced"));
                }
            }
            _ => return,
        }
        core.fetch_now(ResourceId::Config);
    }

    fn on_click(&mut self, core: &mut Core, hit: &Hit) {
        let items = self.items(core);
        kit::click(&mut self.rows, &items, hit);
    }

    fn render(&mut self, f: &mut Frame, area: Rect, core: &Core, out: &mut FrameOut) {
        let items = self.items(core);
        rows_view::render(f, area, core, &items, &mut self.rows, "advanced:", true, out);
    }

    fn context_line(&self, core: &Core) -> Option<String> {
        let items = self.items(core);
        self.rows.focused_row(&items).map(|r| r.help.clone())
    }

    fn animating(&self, _core: &Core) -> bool {
        self.busy || self.pending_level.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn board_override_is_validated_before_sending() {
        assert!(validate_board_override("").is_ok());
        assert!(validate_board_override("rpi-cm4_v2").is_ok());
        assert!(validate_board_override(&"a".repeat(64)).is_ok());
        assert!(validate_board_override(&"a".repeat(65)).is_err());
        assert!(validate_board_override("bad slug").is_err());
        assert!(validate_board_override("x/../y").is_err());
        assert!(validate_board_override("ünï").is_err());
    }
}
