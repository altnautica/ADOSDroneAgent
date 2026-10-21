//! Settings page: ACCESS & SECURITY › Security. Three enforcement flags
//! written through `PUT /api/config` (turning MAVLink WS auth on asks
//! first), plus the read-only API-key state (never the value) and the
//! dashboard PIN status from `GET /api/dashboard/pin/status`.

use ratatui::layout::Rect;
use ratatui::Frame;
use serde_json::{json, Value};

use super::kit;
use crate::app::{Core, FrameOut, Hit};
use crate::keymap::Command;
use crate::overlay::{Overlay, OverlayResult, Ret};
use crate::poll::{ResourceId, Sub};
use crate::settings::page::{Bespoke, BespokeId};
use crate::settings::rows_view::{self, Item, RowsState};
use crate::settings::schema::Kind;
use crate::settings::{config_rows, config_write};
use crate::worker::{Purpose, Reply, Route};

const WS_ENFORCE: &str = "mavlink.ws_proxy_enforce_auth";

/// `(key, label, help)` of each flag.
const FLAGS: &[(&str, &str, &str)] = &[
    (
        WS_ENFORCE,
        "Enforce MAVLink WS auth",
        "When on, the raw MAVLink WebSocket proxy rejects an off-box connection from a paired agent that presents no valid pairing key. On-box and unpaired connections stay open.",
    ),
    (
        "security.hmac_enabled",
        "HMAC replay protection",
        "Require an HMAC signature on authenticated requests. Only effective once an HMAC secret is provisioned.",
    ),
    (
        "security.setup_token_required",
        "Require a setup token",
        "Require an X-ADOS-Setup-Token header on every setup mutation. Off trusts any browser served the setup webapp from this agent's own port.",
    ),
];

fn route() -> Route {
    Route::page(BespokeId::Security.id())
}

fn row_key(key: &str) -> String {
    format!("security:{key}")
}

/// The API-key state from the redacted config value: the agent sends `***`
/// for a set secret and `""` for none.
pub fn api_key_state(v: Option<&Value>) -> &'static str {
    match v {
        Some(Value::String(s)) if s == "***" => "set",
        Some(Value::String(s)) if s.is_empty() => "not set",
        Some(Value::Null) => "not set",
        Some(Value::String(_)) => "set",
        _ => "unknown",
    }
}

#[derive(Default)]
pub struct Page {
    rows: RowsState,
}

impl Page {
    fn items(&self, core: &Core) -> Vec<Item> {
        let mut items = vec![Item::Header("Enforcement".into())];
        if let Some(note) = config_rows::status_note(core) {
            items.push(note);
        }
        for (key, label, help) in FLAGS {
            items.push(Item::Row(config_rows::config_row(core, key, label, help)));
        }

        items.push(Item::Header("Agent API key".into()));
        let config = core.value(&ResourceId::Config);
        let key = config.and_then(|c| crate::app::at(c, "security.api.api_key"));
        let state = if config.is_some() { api_key_state(key) } else { "unknown" };
        items.push(rows_view::info(core, "API key", state));
        items.push(rows_view::note(
            core,
            "The value is never shown here; re-pair this node from Mission Control to rotate it.",
            false,
        ));

        items.push(Item::Header("Dashboard access PIN".into()));
        if let Some(note) = kit::resource_note(core, &ResourceId::PinStatus, "the PIN state") {
            items.push(note);
        } else {
            let pin = core.value(&ResourceId::PinStatus);
            let flag = |k: &str| pin.and_then(|p| p.get(k)).and_then(Value::as_bool) == Some(true);
            items.push(rows_view::info(core, "PIN", if flag("pin_set") { "set" } else { "not set" }));
            if flag("locked") {
                let until = pin
                    .and_then(|p| p.get("locked_until"))
                    .and_then(Value::as_f64)
                    .map(|s| kit::utc(s as i64))
                    .unwrap_or_else(|| "unknown".into());
                items.push(rows_view::note(core, format!("Locked until {until}"), true));
            }
            items.push(rows_view::note(
                core,
                "Set or reset the PIN from the dashboard access splash or Mission Control's dashboard-access card.",
                false,
            ));
        }
        items
    }

    fn toggle(&mut self, core: &mut Core) {
        let items = self.items(core);
        let Some(row) = self.rows.focused_row(&items).cloned() else { return };
        if row.read_only || row.kind != Kind::Bool {
            return;
        }
        let next = !row.value.as_ref().and_then(Value::as_bool).unwrap_or(false);
        if row.key == WS_ENFORCE && next {
            core.open(Overlay::confirm(
                Ret::new(route(), "ws_enforce"),
                "Enforce MAVLink WS authentication?",
                vec![
                    "A paired agent will refuse an off-box MAVLink WebSocket connection that carries no valid pairing key.".into(),
                    "Make sure any ground station that talks to this node over the WebSocket bridge is paired first, or it will lose the link.".into(),
                ],
            ));
            return;
        }
        config_write::submit(core, route(), &row.key, json!(next), &row_key(&row.key));
    }
}

impl Bespoke for Page {
    fn subs(&self, core: &Core) -> Vec<Sub> {
        let mut subs = config_rows::subs(core);
        subs.push(Sub::every_ms(ResourceId::PinStatus, "/api/dashboard/pin/status", 10_000));
        subs
    }

    fn on_command(&mut self, core: &mut Core, cmd: Command) -> bool {
        let items = self.items(core);
        if kit::nav(&mut self.rows, &items, cmd) {
            return true;
        }
        match cmd {
            Command::Left | Command::Right | Command::Activate => self.toggle(core),
            _ => return false,
        }
        true
    }

    fn on_overlay(&mut self, core: &mut Core, ret: Ret, result: OverlayResult) {
        if ret.tag == "ws_enforce" && result == OverlayResult::Confirmed {
            config_write::submit(core, route(), WS_ENFORCE, json!(true), &row_key(WS_ENFORCE));
        }
    }

    fn on_reply(&mut self, core: &mut Core, reply: Reply) {
        if let Purpose::ConfigSet { key, row } = &reply.purpose {
            let label = FLAGS
                .iter()
                .find(|(k, _, _)| k == key)
                .map(|(_, l, _)| *l)
                .unwrap_or(key.as_str())
                .to_string();
            config_write::on_reply(core, &label, row, &reply.result);
        }
    }

    fn on_click(&mut self, core: &mut Core, hit: &Hit) {
        let items = self.items(core);
        kit::click(&mut self.rows, &items, hit);
    }

    fn render(&mut self, f: &mut Frame, area: Rect, core: &Core, out: &mut FrameOut) {
        let items = self.items(core);
        rows_view::render(f, area, core, &items, &mut self.rows, "security:", true, out);
    }

    fn context_line(&self, core: &Core) -> Option<String> {
        let items = self.items(core);
        self.rows.focused_row(&items).map(|r| r.help.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_api_key_state_never_shows_the_value() {
        assert_eq!(api_key_state(Some(&json!("***"))), "set");
        assert_eq!(api_key_state(Some(&json!(""))), "not set");
        assert_eq!(api_key_state(Some(&Value::Null)), "not set");
        assert_eq!(api_key_state(None), "unknown");
    }
}
