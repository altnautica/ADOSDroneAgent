//! Settings page: the setup hotspot. Turning it on or off goes through the
//! setup apply (`network.hotspot_enabled`); the SSID and passphrase are shown
//! read-only.

use ratatui::layout::Rect;
use ratatui::Frame;
use serde_json::{json, Value};

use super::kit;
use crate::app::{at, Core, FrameOut, Hit};
use crate::keymap::Command;
use crate::overlay::{Overlay, OverlayResult, Ret};
use crate::poll::{ResourceId, Sub};
use crate::settings::apply_result;
use crate::settings::page::{Bespoke, BespokeId};
use crate::settings::row::EditRow;
use crate::settings::rows_view::{self, note, Item, RowsState};
use crate::settings::schema::Kind;
use crate::worker::{Purpose, Reply, Request, Route};

const ROUTE: Route = Route::page("hotspot");

const HINT: &str = "Spins up a captive AP on wlan0 so a phone or laptop can join the agent over Wi-Fi and finish onboarding without a wired connection.";
const REMOTE_PASS: &str = "shown on the agent only";
const REMOTE_PASS_HINT: &str = "The passphrase is read from a file on the agent's own disk, so a remote terminal cannot show it. Open the TUI on the agent or the agent's web dashboard to see it.";
const WARNING: &str = "Enabling this disconnects any active Wi-Fi client link (the radio can host one or the other, not both).";

#[derive(Default)]
pub struct Page {
    rows: RowsState,
}

/// A read-only row; only Enabled is editable.
fn row(key: &str, label: &str, help: &str, kind: Kind, value: Option<Value>) -> EditRow {
    EditRow {
        read_only: true,
        ..kit::row(key, label, help, kind, value)
    }
}

impl Page {
    fn enabled(core: &Core) -> Option<bool> {
        core.value(&ResourceId::SetupStatus)
            .and_then(|v| at(v, "network.hotspot_enabled"))
            .and_then(Value::as_bool)
    }

    fn items(&self, core: &Core) -> Vec<Item> {
        let setup = core.value(&ResourceId::SetupStatus);
        let ssid = setup
            .and_then(|v| at(v, "network.hotspot_ssid"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(|s| json!(s));
        let passphrase = if core.target.remote {
            row(
                "passphrase",
                "Passphrase",
                REMOTE_PASS_HINT,
                Kind::Str { pattern: None },
                Some(json!(REMOTE_PASS)),
            )
        } else {
            let pass = core
                .dash()
                .map(|d| d.hotspot_passphrase.clone())
                .filter(|p| !p.is_empty())
                .map(Value::String);
            let mut r = row(
                "passphrase",
                "Passphrase",
                "The access point's passphrase, generated per unit. v reveals it for 10 s; y copies it.",
                Kind::Str { pattern: None },
                pass.or(Some(Value::Null)),
            );
            r.secret = true;
            r
        };
        let mut enabled = row(
            "enabled",
            "Enabled",
            HINT,
            Kind::Bool,
            Self::enabled(core).map(Value::Bool),
        );
        enabled.read_only = false;
        let mut items = vec![Item::Header("Setup hotspot".into())];
        items.extend(kit::resource_note(core, &ResourceId::SetupStatus, "the setup status"));
        items.push(Item::Row(enabled));
        items.push(Item::Row(row(
            "ssid",
            "SSID",
            "The access point name this agent broadcasts.",
            Kind::Str { pattern: None },
            ssid,
        )));
        items.push(Item::Row(passphrase));
        items.push(note(core, WARNING, false));
        items
    }

    fn ask_toggle(core: &mut Core) {
        let Some(on) = Self::enabled(core) else {
            core.toast(crate::toast::ToastKind::Warn, "Hotspot state not read yet");
            return;
        };
        let next = !on;
        let (title, body) = if next {
            ("Turn the hotspot on?", vec![WARNING.to_string()])
        } else {
            (
                "Turn the hotspot off?",
                vec!["Phones and laptops joined to the hotspot lose their connection.".to_string()],
            )
        };
        core.open(Overlay::confirm(
            Ret::new(ROUTE, "toggle").with(json!(next)),
            title,
            body,
        ));
    }
}

impl Bespoke for Page {
    fn subs(&self, _core: &Core) -> Vec<Sub> {
        vec![kit::setup_sub()]
    }

    fn on_command(&mut self, core: &mut Core, cmd: Command) -> bool {
        let items = self.items(core);
        if kit::nav(&mut self.rows, &items, cmd) {
            return true;
        }
        let focused = self.rows.focused_key(&items).map(str::to_string);
        match (cmd, focused.as_deref()) {
            (Command::Left | Command::Right | Command::Activate, Some("enabled")) => Self::ask_toggle(core),
            (Command::Reveal, _) => {
                self.rows.reveal(&items, core.now);
            }
            (Command::Copy, Some("passphrase")) if core.target.remote => {
                core.toast(crate::toast::ToastKind::Info, "The hotspot passphrase is shown on the agent only");
            }
            (Command::Copy, Some(key)) => {
                if let Some(Value::String(s)) = kit::find_row(&items, key).and_then(|r| r.value.clone()) {
                    core.copy(&s);
                }
            }
            (Command::Left | Command::Right | Command::Activate, _) => {}
            _ => return false,
        }
        true
    }

    fn on_overlay(&mut self, core: &mut Core, ret: Ret, result: OverlayResult) {
        if ret.tag == "toggle" && result == OverlayResult::Confirmed {
            let on = ret.data.as_bool().unwrap_or(false);
            core.submit(
                Request::new(
                    "POST",
                    "/api/v1/setup/apply",
                    ROUTE,
                    Purpose::SetupApply { section: "network" },
                )
                .body(json!({ "network": { "hotspot_enabled": on } })),
            );
        }
    }

    fn on_reply(&mut self, core: &mut Core, reply: Reply) {
        if let Purpose::SetupApply { .. } = reply.purpose {
            let ok = matches!(&reply.result, Ok(b) if apply_result::succeeded(b, "network"));
            apply_result::show(core, "Hotspot", &reply.result);
            core.flash(format!("{}enabled", kit::prefix(BespokeId::Hotspot.id())), ok);
            core.fetch_now(ResourceId::SetupStatus);
            core.poller.refresh();
        }
    }

    fn on_click(&mut self, core: &mut Core, hit: &Hit) {
        let items = self.items(core);
        kit::click(&mut self.rows, &items, hit);
    }

    fn render(&mut self, f: &mut Frame, area: Rect, core: &Core, out: &mut FrameOut) {
        let items = self.items(core);
        rows_view::render(
            f,
            area,
            core,
            &items,
            &mut self.rows,
            &kit::prefix(BespokeId::Hotspot.id()),
            true,
            out,
        );
    }

    fn context_line(&self, core: &Core) -> Option<String> {
        let items = self.items(core);
        self.rows.focused_row(&items).map(|r| r.help.clone())
    }

    fn animating(&self, core: &Core) -> bool {
        self.rows.revealing(core.now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::tests::test_app;

    fn passphrase(page: &Page, core: &Core) -> EditRow {
        kit::find_row(&page.items(core), "passphrase").cloned().expect("the passphrase row")
    }

    #[test]
    fn a_remote_target_shows_the_passphrase_as_agent_only_text() {
        let mut app = test_app();
        let page = Page::default();
        let local = passphrase(&page, &app.core);
        assert!(local.secret && local.read_only);
        app.core.target.remote = true;
        let remote = passphrase(&page, &app.core);
        assert!(!remote.secret, "nothing to mask or reveal");
        assert!(remote.read_only);
        assert_eq!(remote.value, Some(json!(REMOTE_PASS)));
    }
}
