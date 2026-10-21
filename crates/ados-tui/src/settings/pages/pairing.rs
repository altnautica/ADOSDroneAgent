//! Settings page: ACCESS & SECURITY › Pairing. The cloud-pair state from
//! `GET /api/pairing/info`, the pairing code in large accent text, `c` to
//! accept a code issued by Mission Control (`POST /api/pairing/accept`), `U`
//! to unpair (typed confirm, `POST /api/pairing/unpair`), and `y` to copy the
//! code.

use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::Frame;
use serde_json::{json, Value};

use super::kit;
use crate::app::{Core, FrameOut, Hit};
use crate::keymap::{Command, PageKeys};
use crate::overlay::{Overlay, OverlayResult, Ret};
use crate::poll::{ResourceId, Sub};
use crate::settings::page::{Bespoke, BespokeId};
use crate::settings::rows_view::{self, Item, RowsState};
use crate::toast::ToastKind;
use crate::worker::{Purpose, Reply, Request, Route};

fn route() -> Route {
    Route::page(BespokeId::Pairing.id())
}

fn validate_code(s: &str) -> Result<(), String> {
    if s.trim().is_empty() {
        Err("Enter the code shown in Mission Control".into())
    } else {
        Ok(())
    }
}

#[derive(Default)]
pub struct Page {
    rows: RowsState,
    busy: bool,
}

fn info(core: &Core) -> Option<&Value> {
    core.value(&ResourceId::PairingInfo)
}

fn paired(core: &Core) -> bool {
    info(core).and_then(|v| v.get("paired")).and_then(Value::as_bool) == Some(true)
}

fn code(core: &Core) -> Option<String> {
    kit::text(info(core), "pairing_code").map(str::to_string)
}

/// The code spaced out so it reads at a glance: `A B C 1 2 3`.
fn spaced(code: &str) -> String {
    code.chars().map(|c| c.to_string()).collect::<Vec<_>>().join(" ")
}

impl Page {
    fn items(&self, core: &Core) -> Vec<Item> {
        let t = &core.theme;
        let v = info(core);
        let mut items = vec![Item::Header("Pairing".into())];
        if let Some(note) = kit::resource_note(core, &ResourceId::PairingInfo, "the pairing state") {
            items.push(note);
        }
        let is_paired = paired(core);
        items.push(Item::Text(Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{:<20}  ", "Paired"), t.dim()),
            if is_paired {
                Span::styled(format!("{} paired", core.sym.ok), t.chip(t.success()))
            } else {
                Span::styled(format!("{} not paired", core.sym.info), t.text())
            },
        ])));
        if let Some(c) = code(core) {
            items.push(Item::Text(Line::from("")));
            items.push(Item::Text(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{:<20}  ", "Pairing code"), t.dim()),
                Span::styled(spaced(&c), t.accent_bold().add_modifier(Modifier::BOLD)),
            ])));
            items.push(rows_view::note(core, "Type this code into Mission Control to claim this agent.", false));
            items.push(Item::Text(Line::from("")));
        }
        if let Some(owner) = kit::text(v, "owner_id") {
            items.push(rows_view::info(core, "Owner", owner.to_string()));
        }
        if let Some(at) = v.and_then(|v| v.get("paired_at")).and_then(Value::as_f64) {
            items.push(rows_view::info(core, "Paired at", kit::utc(at as i64)));
        }
        if let Some(id) = kit::text(v, "device_id") {
            items.push(rows_view::info(core, "Device ID", id.to_string()));
        }
        if let Some(host) = kit::text(v, "mdns_host") {
            items.push(rows_view::info(core, "mDNS host", host.to_string()));
        }
        items.push(Item::Header("Actions".into()));
        if self.busy {
            items.push(rows_view::note(core, format!("{} Working…", core.sym.spinner_frame(core.frame as usize)), false));
        }
        if !is_paired {
            items.push(kit::action(core, "accept", "Accept a code", "c · a code Mission Control issued"));
        }
        if code(core).is_some() {
            items.push(kit::action(core, "copy", "Copy the pairing code", "y"));
        }
        if is_paired {
            items.push(kit::action(core, "unpair", "Unpair this agent", "U · type unpair to confirm"));
        }
        items
    }

    fn accept(core: &mut Core) {
        let mut s = Overlay::input(Ret::new(route(), "accept"), "Accept a pairing code", "Code from Mission Control", "");
        s.validate = Some(validate_code);
        core.open(Overlay::Input(s));
    }

    fn unpair(core: &mut Core) {
        if !paired(core) {
            core.toast(ToastKind::Info, "This agent is not paired");
            return;
        }
        if !core.guard_disarmed() {
            return;
        }
        core.open(Overlay::typed_confirm(
            Ret::new(route(), "unpair"),
            "Unpair this agent?",
            vec![
                "Clears the pairing key and mints a fresh pairing code.".into(),
                "Mission Control loses access until the agent is paired again.".into(),
                "Type unpair to confirm.".into(),
            ],
            "unpair",
        ));
    }

    fn copy_code(core: &mut Core) {
        match code(core) {
            Some(c) => core.copy(&c),
            None => core.toast(ToastKind::Info, "No pairing code (the agent is paired)"),
        }
    }
}

impl Bespoke for Page {
    fn subs(&self, _core: &Core) -> Vec<Sub> {
        vec![Sub::every_ms(ResourceId::PairingInfo, "/api/pairing/info", 5000)]
    }

    fn keys(&self) -> Option<PageKeys> {
        Some(PageKeys::Pairing)
    }

    fn on_command(&mut self, core: &mut Core, cmd: Command) -> bool {
        let items = self.items(core);
        if kit::nav(&mut self.rows, &items, cmd) {
            return true;
        }
        match cmd {
            Command::AcceptCode => Self::accept(core),
            Command::Unpair => Self::unpair(core),
            Command::Copy => Self::copy_code(core),
            Command::Activate => match self.rows.focused_key(&items) {
                Some("accept") => Self::accept(core),
                Some("copy") => Self::copy_code(core),
                Some("unpair") => Self::unpair(core),
                _ => {}
            },
            _ => return false,
        }
        true
    }

    fn on_overlay(&mut self, core: &mut Core, ret: Ret, result: OverlayResult) {
        match (ret.tag, result) {
            ("accept", OverlayResult::Text(code)) => {
                self.busy = true;
                core.submit(
                    Request::new("POST", "/api/pairing/accept", route(), Purpose::Page { tag: "accept", data: Value::Null })
                        .body(json!({ "code": code.trim() })),
                );
            }
            ("unpair", OverlayResult::Confirmed) => {
                if !core.guard_disarmed() {
                    return;
                }
                self.busy = true;
                core.submit(Request::new(
                    "POST",
                    "/api/pairing/unpair",
                    route(),
                    Purpose::Page { tag: "unpair", data: Value::Null },
                ));
            }
            _ => {}
        }
    }

    fn on_reply(&mut self, core: &mut Core, reply: Reply) {
        let Purpose::Page { tag, .. } = reply.purpose else { return };
        self.busy = false;
        match (tag, &reply.result) {
            ("accept", Ok(body)) => {
                let message = body.get("message").and_then(Value::as_str).filter(|m| !m.is_empty());
                if body.get("ok").and_then(Value::as_bool) == Some(true) {
                    core.toast(ToastKind::Success, message.unwrap_or("Paired"));
                } else {
                    let error = body.get("error").and_then(Value::as_str).unwrap_or("failed");
                    core.toast(ToastKind::Error, message.map(str::to_string).unwrap_or_else(|| format!("Pairing failed: {error}")));
                }
            }
            ("accept", Err(e)) => core.toast_error("Pairing failed", e),
            ("unpair", Ok(body)) => {
                let text = match body.get("new_code").and_then(Value::as_str) {
                    Some(c) => format!("Unpaired. New pairing code {c}"),
                    None => "Unpaired".into(),
                };
                core.toast(ToastKind::Success, text);
            }
            ("unpair", Err(e)) => core.toast_error("Unpair failed", e),
            _ => return,
        }
        core.fetch_now(ResourceId::PairingInfo);
    }

    fn on_click(&mut self, core: &mut Core, hit: &Hit) {
        let items = self.items(core);
        kit::click(&mut self.rows, &items, hit);
    }

    fn render(&mut self, f: &mut Frame, area: Rect, core: &Core, out: &mut FrameOut) {
        let items = self.items(core);
        rows_view::render(f, area, core, &items, &mut self.rows, "pairing:", true, out);
    }

    fn context_line(&self, core: &Core) -> Option<String> {
        let items = self.items(core);
        match self.rows.focused_key(&items)? {
            "accept" => Some("Pair by typing a code Mission Control pre-allocated, instead of the other way round".into()),
            "copy" => Some("Copy the pairing code to the clipboard".into()),
            "unpair" => Some("Clear the pairing; refused while the vehicle is armed".into()),
            _ => None,
        }
    }

    fn animating(&self, _core: &Core) -> bool {
        self.busy
    }
}
