//! Settings page: video link tuning. Preset, FEC k/n, MCS and the adaptive
//! tier switch, read from `GET /api/video/config` and written one field per
//! `POST /api/video/config`; knobs the radio did not take come back as
//! warnings.

use ratatui::layout::Rect;
use ratatui::Frame;
use serde_json::{json, Map, Value};

use super::kit::{self, detail_text, text_of};
use crate::app::{at, Core, FrameOut, Hit};
use crate::keymap::Command;
use crate::overlay::{OverlayResult, Ret};
use crate::poll::{ResourceId, Sub};
use crate::settings::page::{Bespoke, BespokeId};
use crate::settings::row;
use crate::settings::rows_view::{self, info, note, Item, RowsState};
use crate::settings::schema::Kind;
use crate::toast::ToastKind;
use crate::worker::{Purpose, Reply, Request, Route};

const ROUTE: Route = Route::page("video_link");

/// `(field, label, help, kind)` for each row; `field` is the POST body key.
fn specs() -> [(&'static str, &'static str, &'static str, Kind); 5] {
    [
        (
            "preset",
            "Preset",
            "A named MCS + FEC trio: conservative favours range, aggressive favours throughput.",
            Kind::Enum(vec![json!("conservative"), json!("balanced"), json!("aggressive")]),
        ),
        ("fec_k", "FEC k", "Data packets per FEC block (1-64).", Kind::Int { min: Some(1), max: Some(64) }),
        (
            "fec_n",
            "FEC n",
            "Total packets per FEC block, data plus recovery (2-128, above k).",
            Kind::Int { min: Some(2), max: Some(128) },
        ),
        ("mcs", "MCS", "Modulation and coding index (0-7). Lower reaches further.", Kind::Int { min: Some(0), max: Some(7) }),
        (
            "auto",
            "Auto",
            "Let the adaptive controller pick the link tier from measured link quality.",
            Kind::Bool,
        ),
    ]
}

/// The current value of a field in the config body.
fn current(cfg: &Value, field: &str) -> Option<Value> {
    let path = match field {
        "preset" => "radio.preset",
        "fec_k" => "radio.fec_k",
        "fec_n" => "radio.fec_n",
        "mcs" => "radio.mcs_index",
        _ => {
            return at(cfg, "adaptive.adaptive_bitrate_enabled")
                .or_else(|| at(cfg, "adaptive.available"))
                .cloned();
        }
    };
    at(cfg, path).cloned()
}

/// The reply's `warnings` list.
pub fn warnings(body: &Value) -> Vec<String> {
    body.get("warnings")
        .and_then(Value::as_array)
        .map(|w| w.iter().filter_map(Value::as_str).map(|s| s.replace('_', " ")).collect())
        .unwrap_or_default()
}

#[derive(Default)]
pub struct Page {
    rows: RowsState,
}

impl Page {
    fn items(&self, core: &Core) -> Vec<Item> {
        let mut items = vec![Item::Header("Video link".into())];
        let Some(cfg) = core.value(&ResourceId::VideoConfig) else {
            items.push(match core.slot(&ResourceId::VideoConfig).and_then(|s| s.error.clone()) {
                Some(e) => note(core, format!("Could not read the video config: {e}"), true),
                None => note(core, "Reading video config…", false),
            });
            return items;
        };
        for (field, label, help, kind) in specs() {
            items.push(Item::Row(kit::row(field, label, help, kind, current(cfg, field))));
        }
        items.push(info(core, "Channel", text_of(at(cfg, "radio.channel"))));
        items.push(info(core, "Band", text_of(at(cfg, "radio.band"))));
        items.push(info(core, "Encoder bitrate", format!("{} kbps", text_of(at(cfg, "encoder.bitrate_kbps")))));
        items
    }

    fn post(core: &mut Core, field: &str, value: Value) {
        let mut body = Map::new();
        body.insert(field.to_string(), value);
        core.submit(
            Request::new(
                "POST",
                "/api/video/config",
                ROUTE,
                Purpose::Page { tag: "video", data: json!(field) },
            )
            .body(Value::Object(body)),
        );
    }
}

impl Bespoke for Page {
    fn subs(&self, _core: &Core) -> Vec<Sub> {
        vec![Sub::every_ms(ResourceId::VideoConfig, "/api/video/config", 15000)]
    }

    fn on_command(&mut self, core: &mut Core, cmd: Command) -> bool {
        let items = self.items(core);
        if kit::nav(&mut self.rows, &items, cmd) {
            return true;
        }
        let Some(row) = self.rows.focused_row(&items).cloned() else {
            return matches!(cmd, Command::Left | Command::Right | Command::Activate);
        };
        match cmd {
            Command::Activate if row.kind != Kind::Bool => {
                kit::open_editor(core, ROUTE, &row);
            }
            Command::Left | Command::Right | Command::Activate => {
                if let Some(next) = row::cycle(&row.kind, row.value.as_ref(), cmd != Command::Left) {
                    Self::post(core, &row.key, next);
                }
            }
            Command::Copy => core.copy(&row::display(&row.kind, row.value.as_ref(), false, true)),
            _ => return false,
        }
        true
    }

    fn on_overlay(&mut self, core: &mut Core, ret: Ret, result: OverlayResult) {
        if ret.tag != kit::CONFIG_EDIT {
            return;
        }
        let items = self.items(core);
        let Some(row) = kit::find_row(&items, ret.data.as_str().unwrap_or_default()).cloned() else {
            return;
        };
        match row::value_from_result(&row.kind, &result) {
            Some(Ok(v)) => Self::post(core, &row.key, v),
            Some(Err(why)) => core.toast(ToastKind::Error, format!("{}: {why}", row.label)),
            None => {}
        }
    }

    fn on_reply(&mut self, core: &mut Core, reply: Reply) {
        let Purpose::Page { tag: "video", data } = &reply.purpose else { return };
        let field = data.as_str().unwrap_or_default();
        let label = specs()
            .into_iter()
            .find(|s| s.0 == field)
            .map(|s| s.1)
            .unwrap_or(field);
        let row = format!("{}{field}", kit::prefix(BespokeId::VideoLink.id()));
        match &reply.result {
            Ok(body) => {
                let warns = warnings(body);
                if warns.is_empty() {
                    core.toast(ToastKind::Success, format!("{label} saved"));
                } else {
                    for w in &warns {
                        core.toast(ToastKind::Warn, format!("{label}: {w}"));
                    }
                }
                core.flash(row, warns.is_empty());
            }
            Err(e) => {
                core.toast(ToastKind::Error, format!("{label}: {}", detail_text(e)));
                core.flash(row, false);
            }
        }
        core.fetch_now(ResourceId::VideoConfig);
    }

    fn on_click(&mut self, core: &mut Core, hit: &Hit) {
        let items = self.items(core);
        kit::click(&mut self.rows, &items, hit);
    }

    fn render(&mut self, f: &mut Frame, area: Rect, core: &Core, out: &mut FrameOut) {
        let items = self.items(core);
        let prefix = kit::prefix(BespokeId::VideoLink.id());
        rows_view::render(f, area, core, &items, &mut self.rows, &prefix, true, out);
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
    fn fields_read_from_the_config_body_and_warnings_list() {
        let cfg = json!({"radio":{"preset":"balanced","fec_k":8,"fec_n":12,"mcs_index":2},"adaptive":{"available":true}});
        assert_eq!(current(&cfg, "preset"), Some(json!("balanced")));
        assert_eq!(current(&cfg, "mcs"), Some(json!(2)));
        assert_eq!(current(&cfg, "auto"), Some(json!(true)));
        let cfg = json!({"adaptive":{"available":true,"adaptive_bitrate_enabled":false}});
        assert_eq!(current(&cfg, "auto"), Some(json!(false)));
        assert_eq!(warnings(&json!({"warnings":["set_mcs_failed"]})), vec!["set mcs failed"]);
        assert!(warnings(&json!({"warnings":[]})).is_empty());
    }

    #[test]
    fn enter_toggles_auto_and_opens_an_editor_for_numbers() {
        let mut app = crate::ui::tests::test_app();
        let core = &mut app.core;
        kit::put(core, ResourceId::VideoConfig, json!({"radio":{"mcs_index":2},"adaptive":{"adaptive_bitrate_enabled":true}}));
        let mut page = Page::default();
        let items = page.items(core);
        page.rows.focus_key(&items, "auto");
        assert!(page.on_command(core, Command::Activate));
        assert!(core.overlays.is_empty(), "a toggle never opens an on/off prompt");
        page.rows.focus_key(&items, "mcs");
        assert!(page.on_command(core, Command::Activate));
        assert_eq!(core.overlays.len(), 1, "a number opens its editor");
    }
}
