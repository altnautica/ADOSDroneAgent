//! Settings page: capture exposure (visor). The world-engine extension owns
//! the capture service; the state and the exposure write go through its
//! plugin route. A 404 means the extension is not installed.

use ratatui::layout::Rect;
use ratatui::Frame;
use serde_json::{json, Map, Value};

use super::kit;
use super::rowkit::{self, detail_text, text_of};
use crate::app::{Core, FrameOut, Hit};
use crate::keymap::Command;
use crate::model::CAPTURE_STATE_PATH;
use crate::overlay::{OverlayResult, Ret};
use crate::poll::{ResourceId, Sub};
use crate::settings::page::{Bespoke, BespokeId};
use crate::settings::row::{self, EditRow};
use crate::settings::rows_view::{self, info, note, Item, RowsState};
use crate::settings::schema::Kind;
use crate::toast::ToastKind;
use crate::worker::{Purpose, Reply, Request, Route};

const ROUTE: Route = Route::page("capture");
const EXPOSURE_PATH: &str = "/api/plugins/com.altnautica.world-engine/x/atlas/capture/exposure";
const MISSING: &str = "Install the World Engine extension to configure capture";
const INCOMPLETE: &str = "Exposure, gain and mode are all needed before the camera takes a change.";

/// The four exposure fields, in row order.
const FIELDS: [&str; 4] = ["locked", "exposure_us", "gain", "mode"];

#[derive(Default)]
pub struct Page {
    rows: RowsState,
    /// Edits not yet accepted (the camera needs all four fields set).
    draft: Option<Map<String, Value>>,
}

fn spec(field: &str) -> (&'static str, &'static str, Kind) {
    match field {
        "locked" => ("Lock exposure", "Hold exposure and gain fixed while capturing.", Kind::Bool),
        "exposure_us" => ("Exposure (µs)", "Manual exposure time in microseconds.", Kind::Int { min: Some(1), max: None }),
        "gain" => ("Gain", "Sensor gain as a multiplier (1 = the driver default).", Kind::Num { min: Some(0.01), max: None }),
        _ => (
            "Mode",
            "Motion profile the exposure is tuned for: walking, slow or short.",
            Kind::Enum(vec![json!("walking"), json!("slow"), json!("short")]),
        ),
    }
}

/// Whether an exposure body is complete enough to send.
pub fn complete(e: &Map<String, Value>) -> bool {
    e.get("exposure_us").and_then(Value::as_f64).is_some_and(|v| v > 0.0)
        && e.get("gain").and_then(Value::as_f64).is_some_and(|v| v > 0.0)
        && e.get("mode").is_some_and(Value::is_string)
}

/// The PUT body: exactly the four fields, nulls for the unknown ones.
pub fn exposure_body(e: &Map<String, Value>) -> Value {
    let mut out = Map::new();
    for f in FIELDS {
        let v = e.get(f).cloned().unwrap_or(if f == "locked" { json!(false) } else { Value::Null });
        out.insert(f.into(), v);
    }
    Value::Object(out)
}

impl Page {
    fn missing(core: &Core) -> bool {
        core.slot(&ResourceId::CaptureState).and_then(|s| s.code) == Some(404)
            && core.value(&ResourceId::CaptureState).is_none()
    }

    /// The live exposure merged under the draft.
    fn exposure(&self, core: &Core) -> Option<Map<String, Value>> {
        let live = core
            .value(&ResourceId::CaptureState)?
            .get("exposure")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let mut merged = live;
        if let Some(d) = &self.draft {
            merged.extend(d.clone());
        }
        Some(merged)
    }

    fn items(&self, core: &Core) -> Vec<Item> {
        let mut items = vec![Item::Header("Capture exposure".into())];
        if Self::missing(core) {
            items.push(note(core, MISSING, true));
            return items;
        }
        let Some(exp) = self.exposure(core) else {
            items.push(match core.slot(&ResourceId::CaptureState).and_then(|s| s.error.clone()) {
                Some(e) => note(core, format!("Could not read the capture state: {e}"), true),
                None => note(core, "Loading…", false),
            });
            return items;
        };
        let state = core.value(&ResourceId::CaptureState);
        items.push(info(core, "State", text_of(state.and_then(|s| s.get("state")))));
        for f in FIELDS {
            let (label, help, kind) = spec(f);
            items.push(Item::Row(EditRow {
                key: f.into(),
                label: label.into(),
                help: help.into(),
                kind,
                value: Some(exp.get(f).cloned().unwrap_or(Value::Null)),
                default: None,
                secret: false,
                nullable: false,
                read_only: false,
            }));
        }
        let locked = exp.get("locked").and_then(Value::as_bool) == Some(true);
        if locked && exp.get("applied").and_then(Value::as_bool) == Some(false) {
            items.push(note(core, "requested, not applied", true));
        }
        if self.draft.is_some() && !complete(&exp) {
            items.push(note(core, INCOMPLETE, true));
        }
        items
    }

    /// Stage `field = value`, sending the whole exposure once it is complete.
    fn set(&mut self, core: &mut Core, field: &str, value: Value) {
        self.draft.get_or_insert_with(Map::new).insert(field.into(), value);
        let Some(exp) = self.exposure(core) else { return };
        if complete(&exp) {
            core.submit(
                Request::new("PUT", EXPOSURE_PATH, ROUTE, Purpose::Page { tag: "exposure", data: json!(field) })
                    .body(exposure_body(&exp)),
            );
        }
    }
}

impl Bespoke for Page {
    fn subs(&self, _core: &Core) -> Vec<Sub> {
        vec![Sub::every_ms(ResourceId::CaptureState, CAPTURE_STATE_PATH, 3000)]
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
            Command::Left | Command::Right => {
                if let Some(next) = row::cycle(&row.kind, row.value.as_ref(), cmd == Command::Right) {
                    self.set(core, &row.key, next);
                }
            }
            Command::Activate => {
                if row.kind == Kind::Bool {
                    let next = !row.value.as_ref().and_then(Value::as_bool).unwrap_or(false);
                    self.set(core, &row.key, json!(next));
                } else {
                    rowkit::config_edit(core, ROUTE, &row);
                }
            }
            Command::Copy => core.copy(&row::display(&row.kind, row.value.as_ref(), false, true)),
            _ => return false,
        }
        true
    }

    fn on_overlay(&mut self, core: &mut Core, ret: Ret, result: OverlayResult) {
        if ret.tag != rowkit::CONFIG_EDIT {
            return;
        }
        let items = self.items(core);
        let Some(row) = rowkit::find_row(&items, ret.data.as_str().unwrap_or_default()).cloned() else {
            return;
        };
        match row::value_from_result(&row.kind, &result) {
            Some(Ok(v)) => self.set(core, &row.key, v),
            Some(Err(why)) => core.toast(ToastKind::Error, format!("{}: {why}", row.label)),
            None => {}
        }
    }

    fn on_reply(&mut self, core: &mut Core, reply: Reply) {
        let Purpose::Page { tag: "exposure", data } = &reply.purpose else { return };
        let field = data.as_str().unwrap_or_default();
        let row = format!("{}{field}", rowkit::prefix(BespokeId::Capture.id()));
        self.draft = None;
        match &reply.result {
            Ok(_) => {
                core.toast(ToastKind::Success, "Exposure updated.");
                core.flash(row, true);
            }
            Err(e) if e.code() == Some(404) => core.toast(ToastKind::Error, MISSING),
            Err(e) => {
                core.toast(ToastKind::Error, format!("Exposure: {}", detail_text(e)));
                core.flash(row, false);
            }
        }
        core.fetch_now(ResourceId::CaptureState);
    }

    fn on_click(&mut self, core: &mut Core, hit: &Hit) {
        let items = self.items(core);
        kit::click(&mut self.rows, &items, hit);
    }

    fn render(&mut self, f: &mut Frame, area: Rect, core: &Core, out: &mut FrameOut) {
        let items = self.items(core);
        let prefix = rowkit::prefix(BespokeId::Capture.id());
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
    fn exposure_needs_every_field_before_it_is_sent() {
        let mut e: Map<String, Value> = serde_json::from_value(json!({"locked":true,"exposure_us":null,"gain":null,"mode":null,"applied":false})).unwrap();
        assert!(!complete(&e));
        e.insert("exposure_us".into(), json!(4000));
        e.insert("gain".into(), json!(2.0));
        e.insert("mode".into(), json!("walking"));
        assert!(complete(&e));
        assert_eq!(
            exposure_body(&e),
            json!({"locked":true,"exposure_us":4000,"gain":2.0,"mode":"walking"})
        );
    }
}
