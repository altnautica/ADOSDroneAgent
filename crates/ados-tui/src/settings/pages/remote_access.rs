//! Settings page: CLOUD & REMOTE › Remote access. The Cloudflare tunnel state
//! from the setup status, and `t` to paste a tunnel token (or the whole
//! `cloudflared service install` command) →
//! `POST /api/v1/setup/remote-access/cloudflare {token_or_script}`.

use ratatui::layout::Rect;
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
    Route::page(BespokeId::RemoteAccess.id())
}

fn non_empty(v: &str) -> Result<(), String> {
    if v.trim().is_empty() {
        Err("Paste a token".into())
    } else {
        Ok(())
    }
}

#[derive(Default)]
pub struct Page {
    rows: RowsState,
    busy: bool,
}

fn yes_no(v: Option<&Value>) -> &'static str {
    match v.and_then(Value::as_bool) {
        Some(true) => "yes",
        Some(false) => "no",
        None => "unknown",
    }
}

impl Page {
    fn items(&self, core: &Core) -> Vec<Item> {
        let t = &core.theme;
        let s = kit::setup(core);
        let ra = s.and_then(|v| v.get("remote_access"));
        let mut items = vec![Item::Header("Cloudflare tunnel".into())];
        if let Some(note) = kit::resource_note(core, &ResourceId::SetupStatus, "the setup status") {
            items.push(note);
        }
        items.push(rows_view::info(core, "Provider", kit::text(s, "remote_access.provider").unwrap_or("none")));
        items.push(rows_view::info(core, "Enabled", yes_no(ra.and_then(|r| r.get("enabled")))));
        items.push(rows_view::info(core, "Token configured", yes_no(ra.and_then(|r| r.get("configured")))));
        let status = kit::text(s, "remote_access.status").unwrap_or("disabled");
        let (glyph, color) = match status {
            "running" => (core.sym.ok, t.success()),
            "error" => (core.sym.err, t.error()),
            "stopped" => (core.sym.warn, t.warning()),
            _ => (core.sym.info, t.muted()),
        };
        items.push(Item::Text(Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{:<20}  ", "Status"), t.dim()),
            Span::styled(format!("{glyph} {status}"), ratatui::style::Style::default().fg(color)),
        ])));
        if let Some(err) = kit::text(s, "remote_access.error") {
            items.push(rows_view::note(core, err.to_string(), true));
        }
        let urls: Vec<String> = ra
            .and_then(|r| r.get("public_urls"))
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default();
        if !urls.is_empty() {
            items.push(Item::Header("Public URLs".into()));
            for (i, u) in urls.iter().enumerate() {
                items.push(Item::Entry {
                    key: format!("url{i}"),
                    line: Line::from(Span::styled(u.clone(), t.text())),
                });
            }
        }
        let detail = if self.busy {
            format!("{} Installing…", core.sym.spinner_frame(core.frame as usize))
        } else {
            "t · a stored token is never shown".into()
        };
        items.push(kit::action(core, "token", "Set tunnel token", &detail));
        items
    }

    fn ask_token(core: &mut Core) {
        let mut s = Overlay::input(
            Ret::new(route(), "token"),
            "Tunnel token",
            "Paste a Cloudflare tunnel token, or the whole cloudflared service install command",
            "",
        );
        s.masked = true;
        s.validate = Some(non_empty);
        s.hint = Some("The agent stores it, enables the tunnel and restarts it".into());
        core.open(Overlay::Input(s));
    }

    fn url(&self, core: &Core, items: &[Item]) -> Option<String> {
        let i: usize = self.rows.focused_key(items)?.strip_prefix("url")?.parse().ok()?;
        kit::setup(core)?
            .get("remote_access")?
            .get("public_urls")?
            .get(i)?
            .as_str()
            .map(str::to_string)
    }
}

impl Bespoke for Page {
    fn subs(&self, _core: &Core) -> Vec<Sub> {
        vec![kit::setup_sub()]
    }

    fn keys(&self) -> Option<PageKeys> {
        Some(PageKeys::RemoteAccess)
    }

    fn on_command(&mut self, core: &mut Core, cmd: Command) -> bool {
        let items = self.items(core);
        if kit::nav(&mut self.rows, &items, cmd) {
            return true;
        }
        match cmd {
            Command::PasteToken => Self::ask_token(core),
            Command::Activate => match self.url(core, &items) {
                Some(u) => core.copy(&u),
                None => Self::ask_token(core),
            },
            Command::Copy => match self.url(core, &items) {
                Some(u) => core.copy(&u),
                None => return false,
            },
            _ => return false,
        }
        true
    }

    fn on_overlay(&mut self, core: &mut Core, ret: Ret, result: OverlayResult) {
        if let ("token", OverlayResult::Text(token)) = (ret.tag, result) {
            self.busy = true;
            core.submit(
                Request::new(
                    "POST",
                    "/api/v1/setup/remote-access/cloudflare",
                    route(),
                    Purpose::Page {
                        tag: "token",
                        data: Value::Null,
                    },
                )
                .body(json!({ "token_or_script": token.trim() })),
            );
        }
    }

    fn on_reply(&mut self, core: &mut Core, reply: Reply) {
        if !matches!(reply.purpose, Purpose::Page { tag: "token", .. }) {
            return;
        }
        self.busy = false;
        match &reply.result {
            Ok(body) => {
                let msg = body.get("message").and_then(Value::as_str).filter(|m| !m.is_empty());
                if body.get("ok").and_then(Value::as_bool) == Some(true) {
                    core.toast(ToastKind::Success, msg.unwrap_or("Cloudflare tunnel token installed."));
                    core.flash("remote_access:token", true);
                } else {
                    core.toast(ToastKind::Error, msg.unwrap_or("The token was not accepted."));
                    core.flash("remote_access:token", false);
                }
            }
            Err(e) => {
                core.toast_error("Could not install the tunnel token", e);
                core.flash("remote_access:token", false);
            }
        }
        core.fetch_now(ResourceId::SetupStatus);
    }

    fn on_click(&mut self, core: &mut Core, hit: &Hit) {
        let items = self.items(core);
        kit::click(&mut self.rows, &items, hit);
    }

    fn render(&mut self, f: &mut Frame, area: Rect, core: &Core, out: &mut FrameOut) {
        let items = self.items(core);
        rows_view::render(f, area, core, &items, &mut self.rows, "remote_access:", true, out);
    }

    fn context_line(&self, core: &Core) -> Option<String> {
        let items = self.items(core);
        match self.rows.focused_key(&items)? {
            "token" => Some("Install a remotely managed Cloudflare Tunnel; a stored token is never shown".into()),
            _ => Some("Enter or y copies the URL".into()),
        }
    }

    fn animating(&self, _core: &Core) -> bool {
        self.busy
    }
}
