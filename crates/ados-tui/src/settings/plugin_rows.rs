//! Extension config rows: the editable parameters a plugin manifest declares
//! (`parameters` / `settings[].parameters`), read from
//! `GET /api/plugins/{id}/config` and written with `PUT /api/plugins/{id}/config`.
//! Shared by the Extensions tab config pane and the per-plugin Settings pages.
//!
//! Overlays this pane opens carry `Ret{tag: "plugin.config", data: {plugin,
//! key}}`; its writes carry `Purpose::Page{tag: "plugin.config", data:
//! {plugin, key}}`, so a host screen routes both back by plugin id.

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use serde_json::{json, Map, Value};

use crate::api_path::seg;
use crate::app::{Core, FrameOut, Hit};
use crate::ext::params::{self, ParamDef};
use crate::ext::state;
use crate::keymap::Command;
use crate::overlay::{OverlayResult, Ret};
use crate::poll::{ResourceId, Sub};
use crate::settings::page::Bespoke;
use crate::settings::row::{cycle, display, edit_overlay, value_from_result, EditRow};
use crate::settings::rows_view::{self, Item, RowsState};
use crate::settings::schema::Kind;
use crate::toast::ToastKind;
use crate::worker::{Purpose, Reply, Request, Route};

/// The tag of every overlay and request the pane owns.
pub const TAG: &str = "plugin.config";
/// How often a plugin's detail (its manifest) is re-read.
pub const DETAIL_MS: u64 = 5_000;
/// How often the live config values are re-read.
pub const CONFIG_MS: u64 = 5_000;

/// The config pane of one plugin.
pub struct PluginConfigPane {
    plugin_id: String,
    route: Route,
    rows: RowsState,
}

impl PluginConfigPane {
    /// A pane for `plugin_id` whose replies and overlays route to `route`.
    pub fn new(plugin_id: String, route: Route) -> Self {
        Self {
            plugin_id,
            route,
            rows: RowsState::default(),
        }
    }

    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    pub fn route(&self) -> Route {
        self.route
    }

    /// The declared parameters (empty until the detail arrives).
    pub fn defs(&self, core: &Core) -> Vec<ParamDef> {
        core.value(&ResourceId::PluginDetail(self.plugin_id.clone()))
            .map(params::defs)
            .unwrap_or_default()
    }

    fn values(&self, core: &Core) -> Map<String, Value> {
        core.value(&ResourceId::PluginConfig(self.plugin_id.clone()))
            .and_then(|v| v.get("values"))
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default()
    }

    fn locale<'a>(&self, core: &'a Core) -> Option<&'a Map<String, Value>> {
        core.value(&ResourceId::PluginLocale(self.plugin_id.clone()))
            .and_then(Value::as_object)
    }

    /// Whether the plugin declares any parameter.
    pub fn has_rows(&self, core: &Core) -> bool {
        !self.defs(core).is_empty()
    }

    /// The row list as drawn.
    pub fn items(&self, core: &Core) -> Vec<Item> {
        let defs = self.defs(core);
        params::items(&defs, &self.values(core), self.locale(core))
    }

    fn prefix(&self) -> String {
        format!("plugin:{}:", self.plugin_id)
    }

    fn focused_row(&self, core: &Core) -> Option<EditRow> {
        let items = self.items(core);
        self.rows.focused_row(&items).cloned()
    }

    /// Send `PUT /api/plugins/{id}/config {key, value}`.
    fn write(&self, core: &mut Core, row: &EditRow, value: Value) {
        let data = json!({"plugin": self.plugin_id, "key": row.key, "label": row.label});
        core.submit(
            Request::new(
                "PUT",
                format!("/api/plugins/{}/config", seg(&self.plugin_id)),
                self.route(),
                Purpose::Page { tag: TAG, data },
            )
            .body(json!({"key": row.key, "value": value})),
        );
    }

    fn ret(&self, key: &str) -> Ret {
        Ret::new(self.route(), TAG).with(json!({"plugin": self.plugin_id, "key": key}))
    }

    fn owns(&self, data: &Value) -> bool {
        data.get("plugin").and_then(Value::as_str) == Some(self.plugin_id.as_str())
    }

    /// Draw the rows. `focused` is false while another pane has focus.
    pub fn draw(
        &mut self,
        f: &mut Frame,
        area: Rect,
        core: &Core,
        focused: bool,
        out: &mut FrameOut,
    ) {
        let mut items = Vec::new();
        let slot = core.slot(&ResourceId::PluginConfig(self.plugin_id.clone()));
        match slot {
            Some(s) if s.value.is_none() && s.error.is_some() => {
                let why = s.error.clone().unwrap_or_default();
                items.push(rows_view::note(
                    core,
                    format!("Config unavailable: {why}"),
                    true,
                ));
            }
            None => items.push(rows_view::note(core, "Reading config…", false)),
            _ => {}
        }
        let rows = self.items(core);
        if rows.is_empty() {
            items.push(rows_view::note(
                core,
                "This extension declares no config parameters.",
                false,
            ));
        }
        items.extend(rows);
        let prefix = self.prefix();
        rows_view::render(f, area, core, &items, &mut self.rows, &prefix, focused, out);
    }
}

/// Resources the Settings sidebar needs to list plugin pages: the install
/// list and the detail of every enabled plugin.
pub fn listing_subs(core: &Core) -> Vec<Sub> {
    let mut subs = vec![Sub::every_ms(ResourceId::Plugins, "/api/plugins", 8_000)];
    let installs = core
        .value(&ResourceId::Plugins)
        .map(state::installs)
        .unwrap_or_default();
    for i in installs.iter().filter(|i| i.enabled()) {
        subs.push(Sub::every_ms(
            ResourceId::PluginDetail(i.id.clone()),
            format!("/api/plugins/{}", seg(&i.id)),
            DETAIL_MS,
        ));
    }
    subs
}

/// Installed, enabled plugins that declare config parameters: `(id, name)`.
pub fn configurable(core: &Core) -> Vec<(String, String)> {
    let installs = core
        .value(&ResourceId::Plugins)
        .map(state::installs)
        .unwrap_or_default();
    installs
        .iter()
        .filter(|i| i.enabled())
        .filter_map(|i| {
            let detail = core.value(&ResourceId::PluginDetail(i.id.clone()))?;
            if params::defs(detail).is_empty() {
                return None;
            }
            let name = crate::app::at(detail, "manifest.name")
                .and_then(Value::as_str)
                .filter(|n| !n.is_empty())
                .unwrap_or(&i.id)
                .to_string();
            Some((i.id.clone(), name))
        })
        .collect()
}

impl Bespoke for PluginConfigPane {
    fn subs(&self, _core: &Core) -> Vec<Sub> {
        let id = &self.plugin_id;
        vec![
            Sub::every_ms(
                ResourceId::PluginDetail(id.clone()),
                format!("/api/plugins/{}", seg(id)),
                DETAIL_MS,
            ),
            Sub::every_ms(
                ResourceId::PluginConfig(id.clone()),
                format!("/api/plugins/{}/config", seg(id)),
                CONFIG_MS,
            ),
            Sub::once(
                ResourceId::PluginLocale(id.clone()),
                format!("/api/plugins/{}/locales/en", seg(id)),
            ),
        ]
    }

    fn on_command(&mut self, core: &mut Core, cmd: Command) -> bool {
        let items = self.items(core);
        match cmd {
            Command::Up => self.rows.step(&items, false),
            Command::Down => self.rows.step(&items, true),
            Command::PageUp => self.rows.jump(&items, -10),
            Command::PageDown => self.rows.jump(&items, 10),
            Command::Top => self.rows.jump(&items, isize::MIN / 2),
            Command::Bottom => self.rows.jump(&items, isize::MAX / 2),
            Command::Left | Command::Right => {
                let Some(row) = self.focused_row(core) else {
                    return false;
                };
                if row.read_only {
                    core.toast(ToastKind::Info, format!("{} is read-only here", row.label));
                    return true;
                }
                match cycle(&row.kind, row.value.as_ref(), cmd == Command::Right) {
                    Some(v) => self.write(core, &row, v),
                    None => return false,
                }
            }
            Command::Activate => {
                let Some(row) = self.focused_row(core) else {
                    return false;
                };
                if row.kind == Kind::Bool && !row.read_only {
                    // A toggle flips in place, like ←/→.
                    if let Some(v) = cycle(&row.kind, row.value.as_ref(), true) {
                        self.write(core, &row, v);
                    }
                    return true;
                }
                match edit_overlay(&row, self.ret(&row.key)) {
                    Some(o) => core.open(o),
                    None => core.toast(ToastKind::Info, format!("{} is read-only here", row.label)),
                }
            }
            Command::Copy => {
                let Some(row) = self.focused_row(core) else {
                    return false;
                };
                let text = display(&row.kind, row.value.as_ref(), false, true);
                core.copy(&text);
            }
            _ => return false,
        }
        true
    }

    fn on_overlay(&mut self, core: &mut Core, ret: Ret, result: OverlayResult) {
        if ret.tag != TAG || !self.owns(&ret.data) {
            return;
        }
        let Some(key) = ret.data.get("key").and_then(Value::as_str) else {
            return;
        };
        let defs = self.defs(core);
        let Some(def) = defs.iter().find(|d| d.key == key) else {
            return;
        };
        let row = params::row(def, &self.values(core), self.locale(core));
        match value_from_result(&row.kind, &result) {
            Some(Ok(v)) => self.write(core, &row, v),
            Some(Err(msg)) => core.toast(ToastKind::Error, format!("{}: {msg}", row.label)),
            None => {}
        }
    }

    fn on_reply(&mut self, core: &mut Core, reply: Reply) {
        let Purpose::Page { tag, data } = &reply.purpose else {
            return;
        };
        if *tag != TAG || !self.owns(data) {
            return;
        }
        let key = data.get("key").and_then(Value::as_str).unwrap_or_default();
        let label = data
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or(key)
            .to_string();
        let flash = format!("{}{key}", self.prefix());
        match &reply.result {
            Ok(_) => {
                core.toast(ToastKind::Success, format!("{label} saved"));
                core.flash(flash, true);
            }
            Err(e) => {
                core.toast_error(&label, e);
                core.flash(flash, false);
            }
        }
        core.fetch_now(ResourceId::PluginConfig(self.plugin_id.clone()));
    }

    fn on_click(&mut self, core: &mut Core, hit: &Hit) {
        if let Hit::Row(i) = hit {
            let items = self.items(core);
            self.rows.select(&items, *i);
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, core: &Core, out: &mut FrameOut) {
        let name = core
            .value(&ResourceId::PluginDetail(self.plugin_id.clone()))
            .and_then(|d| crate::app::at(d, "manifest.name"))
            .and_then(Value::as_str)
            .unwrap_or(&self.plugin_id)
            .to_string();
        let t = &core.theme;
        let head = Line::from(vec![
            Span::styled(name, t.bright()),
            Span::styled(format!("  {}", self.plugin_id), t.dim()),
        ]);
        f.render_widget(
            Paragraph::new(head),
            Rect::new(area.x, area.y, area.width, 1),
        );
        let body = Rect::new(
            area.x,
            area.y + 2.min(area.height),
            area.width,
            area.height.saturating_sub(2),
        );
        self.draw(f, body, core, true, out);
    }

    fn context_line(&self, core: &Core) -> Option<String> {
        self.focused_row(core)
            .map(|r| r.help)
            .filter(|h| !h.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Slot;

    const ID: &str = "com.example.follow";

    fn put(core: &mut Core, id: ResourceId, value: Value) {
        core.res.insert(
            id,
            Slot {
                value: Some(value),
                error: None,
                code: None,
                error_body: None,
                at: core.now,
            },
        );
    }

    #[test]
    fn enter_on_a_bool_parameter_toggles_without_a_prompt() {
        let mut app = crate::ui::tests::test_app();
        let core = &mut app.core;
        put(
            core,
            ResourceId::PluginDetail(ID.into()),
            json!({"manifest": {"id": ID, "gcs": {"contributes": {"parameters": [
                {"key": "auto_start", "schema": {"type": "boolean", "default": false}, "ui": {"label": "Auto start"}},
                {"key": "distance_m", "schema": {"type": "number", "minimum": 3, "maximum": 30}, "ui": {"label": "Follow distance"}}
            ], "settings": []}}}}),
        );
        put(
            core,
            ResourceId::PluginConfig(ID.into()),
            json!({"values": {"auto_start": false, "distance_m": 12}}),
        );
        let mut pane = PluginConfigPane::new(ID.into(), Route::page("plugin"));
        let items = pane.items(core);
        let bool_key = pane_key(&items, "Auto start");
        pane.rows.focus_key(&items, &bool_key);
        assert!(pane.on_command(core, Command::Activate));
        assert!(
            core.overlays.is_empty(),
            "a toggle never opens an on/off prompt"
        );
        let num_key = pane_key(&items, "Follow distance");
        pane.rows.focus_key(&items, &num_key);
        assert!(pane.on_command(core, Command::Activate));
        assert_eq!(core.overlays.len(), 1, "a number opens its editor");
    }

    fn pane_key(items: &[Item], label: &str) -> String {
        items
            .iter()
            .find_map(|i| match i {
                Item::Row(r) if r.label == label => Some(r.key.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no row labelled {label}"))
    }
}
