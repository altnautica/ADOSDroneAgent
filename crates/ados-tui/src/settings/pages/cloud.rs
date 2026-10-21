//! Settings page: CLOUD & REMOTE › Cloud. The cloud posture (`local`,
//! `cloud`, `self_hosted`, plus the self-hosted endpoints), read from the
//! effective config (`server.mode`, `server.self_hosted.*`), is staged on the
//! page; the Apply row sends `POST /api/v1/setup/apply {cloud: {mode,
//! self_hosted?}}` after a confirm and shows the per-section result.

use ratatui::layout::Rect;
use ratatui::Frame;
use serde_json::{json, Value};

use super::kit;
use crate::app::{Core, FrameOut, Hit};
use crate::keymap::Command;
use crate::overlay::{OverlayResult, Ret};
use crate::poll::{ResourceId, Sub};
use crate::settings::apply_result;
use crate::settings::page::{Bespoke, BespokeId};
use crate::settings::row::{cycle, edit_overlay, value_from_result, EditRow};
use crate::settings::rows_view::{self, Item, RowsState};
use crate::settings::schema::Kind;
use crate::toast::ToastKind;
use crate::worker::{Purpose, Reply, Request, Route};

const MODES: &[&str] = &["local", "cloud", "self_hosted"];
const DEFAULT_MQTT_PORT: i64 = 8883;

fn route() -> Route {
    Route::page(BespokeId::Cloud.id())
}

/// The page's staged edits; `None` keeps the agent's value.
#[derive(Default, Clone)]
struct Draft {
    mode: Option<String>,
    url: Option<String>,
    broker: Option<String>,
    port: Option<i64>,
    /// Write-only; empty keeps the stored key.
    api_key: Option<String>,
}

#[derive(Default)]
pub struct Page {
    rows: RowsState,
    draft: Draft,
    busy: bool,
}

/// The agent's current values.
struct Current {
    mode: String,
    url: String,
    broker: String,
    port: i64,
    /// The backend the current mode talks to (empty for local).
    backend: String,
}

fn current(core: &Core) -> Current {
    let c = core.value(&ResourceId::Config);
    let text = |path: &str| kit::text(c, path).unwrap_or_default().to_string();
    let mode = kit::text(c, "server.mode")
        .filter(|m| MODES.contains(m))
        .unwrap_or("local")
        .to_string();
    let url = text("server.self_hosted.url");
    let backend = match mode.as_str() {
        "self_hosted" => url.clone(),
        "cloud" => text("server.cloud.url"),
        _ => String::new(),
    };
    Current {
        url,
        broker: text("server.self_hosted.mqtt_broker"),
        port: c
            .and_then(|v| crate::app::at(v, "server.self_hosted.mqtt_port"))
            .and_then(Value::as_i64)
            .unwrap_or(DEFAULT_MQTT_PORT),
        backend,
        mode,
    }
}

fn mode_help(mode: &str) -> &'static str {
    match mode {
        "cloud" => "Altnautica relay: use the public cloud relay for telemetry and video.",
        "self_hosted" => "Self-hosted: point at your own Convex backend and MQTT broker.",
        _ => "Local only: no cloud uplink. LAN access via /api and the direct dashboard only.",
    }
}

impl Page {
    /// The staged values merged over the agent's.
    fn effective(&self, cur: &Current) -> (String, String, String, i64) {
        let d = &self.draft;
        (
            d.mode.clone().unwrap_or_else(|| cur.mode.clone()),
            d.url.clone().unwrap_or_else(|| cur.url.clone()),
            d.broker.clone().unwrap_or_else(|| cur.broker.clone()),
            d.port.unwrap_or(cur.port),
        )
    }

    fn dirty(&self, core: &Core) -> bool {
        let cur = current(core);
        let (mode, url, broker, port) = self.effective(&cur);
        mode != cur.mode
            || (mode == "self_hosted"
                && (url != cur.url
                    || broker != cur.broker
                    || port != cur.port
                    || self.draft.api_key.as_deref().is_some_and(|k| !k.is_empty())))
    }

    fn items(&self, core: &Core) -> Vec<Item> {
        let cur = current(core);
        let (mode, url, broker, port) = self.effective(&cur);
        let mut items = vec![Item::Header("Cloud posture".into())];
        if let Some(note) = kit::resource_note(core, &ResourceId::Config, "the config") {
            items.push(note);
        }
        let mut row = kit::row(
            "mode",
            "Cloud posture",
            mode_help(&mode),
            kit::enum_of(MODES),
            Some(json!(mode)),
        );
        row.default = Some(json!(cur.mode));
        items.push(Item::Row(row));
        if mode == "self_hosted" {
            items.push(Item::Header("Self-hosted endpoints".into()));
            let str_kind = || Kind::Str { pattern: None };
            let mut r = kit::row(
                "url",
                "Convex URL",
                "The backend URL, e.g. https://convex.example.com",
                str_kind(),
                Some(json!(url)),
            );
            r.default = Some(json!(cur.url));
            items.push(Item::Row(r));
            let mut r = kit::row(
                "broker",
                "MQTT broker",
                "The MQTT broker host, e.g. mqtt.example.com",
                str_kind(),
                Some(json!(broker)),
            );
            r.default = Some(json!(cur.broker));
            items.push(Item::Row(r));
            let port_kind = Kind::Int {
                min: Some(1),
                max: Some(65535),
            };
            let mut r = kit::row(
                "port",
                "Port",
                "The MQTT broker port",
                port_kind,
                Some(json!(port)),
            );
            r.default = Some(json!(cur.port));
            items.push(Item::Row(r));
            let mut r = kit::row(
                "api_key",
                "API key",
                "Write-only. Stored as a root-owned secret on the agent; leave blank to keep the existing key.",
                str_kind(),
                self.draft.api_key.clone().filter(|k| !k.is_empty()).map(|k| json!(k)),
            );
            r.secret = true;
            items.push(Item::Row(r));
        }
        if !cur.backend.is_empty() {
            items.push(rows_view::info(core, "Backend URL", cur.backend.clone()));
        }
        let detail = if self.busy {
            format!("{} Applying…", core.sym.spinner_frame(core.frame as usize))
        } else if self.dirty(core) {
            "unsaved changes".into()
        } else {
            "no changes".into()
        };
        items.push(kit::action(core, "apply", "Apply", &detail));
        items
    }

    fn set(&mut self, key: &str, v: &Value) {
        match key {
            "mode" => self.draft.mode = v.as_str().map(str::to_string),
            "url" => self.draft.url = v.as_str().map(|s| s.trim().to_string()),
            "broker" => self.draft.broker = v.as_str().map(|s| s.trim().to_string()),
            "port" => self.draft.port = v.as_i64(),
            "api_key" => self.draft.api_key = v.as_str().map(str::to_string),
            _ => {}
        }
    }

    fn validate(&self, core: &Core) -> Result<Value, String> {
        let cur = current(core);
        let (mode, url, broker, port) = self.effective(&cur);
        if mode != "self_hosted" {
            return Ok(json!({ "mode": mode }));
        }
        if !(url.starts_with("https://") || url.starts_with("http://")) || url.len() < 10 {
            return Err("Convex URL: enter a full URL, e.g. https://convex.example.com".into());
        }
        if broker.is_empty() {
            return Err("MQTT broker: enter a host".into());
        }
        let mut sh = json!({ "url": url, "mqtt_broker": broker, "mqtt_port": port });
        if let Some(k) = self.draft.api_key.as_deref().filter(|k| !k.is_empty()) {
            sh["api_key"] = json!(k);
        }
        Ok(json!({ "mode": mode, "self_hosted": sh }))
    }

    fn apply(&mut self, core: &mut Core) {
        if !self.dirty(core) {
            core.toast(ToastKind::Info, "No cloud change to apply");
            return;
        }
        match self.validate(core) {
            Err(e) => core.toast(ToastKind::Error, e),
            Ok(section) => {
                let mode = section["mode"].as_str().unwrap_or_default().to_string();
                core.open(crate::overlay::Overlay::confirm(
                    Ret::new(route(), "apply").with(section),
                    "Switch cloud posture?",
                    vec![
                        format!(
                            "The cloud relay client will reconnect using the new posture ({mode})."
                        ),
                        "Existing MQTT and HTTP sessions drop. Pairing is preserved.".into(),
                    ],
                ));
            }
        }
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
        let focused: Option<EditRow> = self.rows.focused_row(&items).cloned();
        match cmd {
            Command::Left | Command::Right => {
                let Some(row) = focused else { return true };
                if let Some(v) = cycle(&row.kind, row.value.as_ref(), cmd == Command::Right) {
                    self.set(&row.key, &v);
                }
            }
            Command::Activate => match (self.rows.focused_key(&items), focused) {
                (Some("apply"), _) => self.apply(core),
                (_, Some(row)) => {
                    if let Some(o) =
                        edit_overlay(&row, Ret::new(route(), "edit").with(json!(row.key)))
                    {
                        core.open(o);
                    }
                }
                _ => {}
            },
            Command::Reveal => return self.rows.reveal(&items, core.now),
            _ => return false,
        }
        true
    }

    fn on_overlay(&mut self, core: &mut Core, ret: Ret, result: OverlayResult) {
        match ret.tag {
            "edit" => {
                let key = ret.data.as_str().unwrap_or_default().to_string();
                let items = self.items(core);
                let kind = items.iter().find_map(|i| match i {
                    Item::Row(r) if r.key == key => Some(r.kind.clone()),
                    _ => None,
                });
                match kind.and_then(|k| value_from_result(&k, &result)) {
                    Some(Ok(v)) => self.set(&key, &v),
                    Some(Err(e)) => core.toast(ToastKind::Error, e),
                    None => {}
                }
            }
            "apply" if result == OverlayResult::Confirmed => {
                self.busy = true;
                core.submit(
                    Request::new(
                        "POST",
                        "/api/v1/setup/apply",
                        route(),
                        Purpose::SetupApply { section: "cloud" },
                    )
                    .body(json!({ "cloud": ret.data })),
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
        if let Some(body) = apply_result::show(core, "Cloud", &reply.result) {
            let ok = apply_result::succeeded(body, "cloud");
            if ok {
                self.draft = Draft::default();
            }
            core.flash("cloud:mode", ok);
        }
        core.fetch_now(ResourceId::Config);
    }

    fn on_click(&mut self, core: &mut Core, hit: &Hit) {
        let items = self.items(core);
        kit::click(&mut self.rows, &items, hit);
    }

    fn render(&mut self, f: &mut Frame, area: Rect, core: &Core, out: &mut FrameOut) {
        let items = self.items(core);
        rows_view::render(f, area, core, &items, &mut self.rows, "cloud:", true, out);
    }

    fn context_line(&self, core: &Core) -> Option<String> {
        let items = self.items(core);
        match self.rows.focused_key(&items)? {
            "apply" => Some("Send the staged cloud posture to the agent".into()),
            _ => self.rows.focused_row(&items).map(|r| r.help.clone()),
        }
    }

    fn animating(&self, core: &Core) -> bool {
        self.busy || self.rows.revealing(core.now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_apply_keeps_the_configured_broker_and_port() {
        let mut app = crate::ui::tests::test_app();
        let core = &mut app.core;
        kit::put(
            core,
            ResourceId::Config,
            json!({"server": {
                "mode": "self_hosted",
                "cloud": {"url": "https://cloud.example.com"},
                "self_hosted": {"url": "https://convex.example.com", "mqtt_broker": "mqtt.example.com", "mqtt_port": 8884, "api_key": "***"}
            }}),
        );
        let cur = current(core);
        assert_eq!(
            (cur.mode.as_str(), cur.broker.as_str(), cur.port),
            ("self_hosted", "mqtt.example.com", 8884)
        );
        assert_eq!(cur.backend, "https://convex.example.com");
        let mut page = Page::default();
        page.draft.url = Some("https://convex2.example.com".into());
        assert!(page.dirty(core));
        assert_eq!(
            page.validate(core),
            Ok(json!({"mode": "self_hosted", "self_hosted": {
                "url": "https://convex2.example.com", "mqtt_broker": "mqtt.example.com", "mqtt_port": 8884
            }}))
        );
    }

    #[test]
    fn switching_to_self_hosted_starts_from_the_self_hosted_block() {
        let mut app = crate::ui::tests::test_app();
        let core = &mut app.core;
        kit::put(
            core,
            ResourceId::Config,
            json!({"server": {"mode": "cloud", "cloud": {"url": "https://cloud.example.com"},
                   "self_hosted": {"url": "", "mqtt_broker": "", "mqtt_port": 8883}}}),
        );
        let cur = current(core);
        assert_eq!(cur.backend, "https://cloud.example.com");
        assert_eq!(cur.url, "", "the cloud URL is not the self-hosted URL");
    }
}
