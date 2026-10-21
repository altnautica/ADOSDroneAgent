//! Settings page: sensors. The sensor hub card and clock sources (read-only)
//! and `l` to load a `rig.json` from a path on the machine running this
//! terminal and store it on the agent.

use std::path::Path;

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::Frame;
use serde_json::{json, Value};

use super::kit;
use super::rowkit::{self, detail_text, error_code, text_of};
use crate::app::{Core, FrameOut, Hit};
use crate::keymap::{Command, PageKeys};
use crate::overlay::{Overlay, OverlayResult, Ret};
use crate::poll::{ResourceId, Sub};
use crate::settings::page::{Bespoke, BespokeId};
use crate::settings::rows_view::{self, info, note, Item, RowsState};
use crate::toast::ToastKind;
use crate::worker::{Purpose, Reply, Request, Route};

const ROUTE: Route = Route::page("sensors");

#[derive(Default)]
pub struct Page {
    rows: RowsState,
}

/// Read and parse a rig file: it must be a JSON object.
pub fn load_rig(path: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("Could not read {}: {e}", path.display()))?;
    let doc: Value = serde_json::from_str(&text).map_err(|e| format!("Not valid JSON: {e}"))?;
    if !doc.is_object() {
        return Err("The rig file must be a JSON object.".into());
    }
    Ok(doc)
}

/// A σ in ns as a readable figure; negative means unknown.
fn sigma(ns: Option<f64>) -> String {
    match ns {
        Some(n) if n >= 1_000_000.0 => format!("{:.2} ms", n / 1e6),
        Some(n) if n >= 0.0 => format!("{:.0} µs", n / 1e3),
        _ => "—".into(),
    }
}

impl Page {
    fn hub_items(core: &Core, items: &mut Vec<Item>) {
        items.push(Item::Header("Sensor hub".into()));
        let Some(s) = core.value(&ResourceId::SensorsStatus) else {
            items.push(match core.slot(&ResourceId::SensorsStatus).and_then(|s| s.error.clone()) {
                Some(e) => note(core, format!("Could not read sensors: {e}"), true),
                None => note(core, "Reading sensors…", false),
            });
            return;
        };
        match s.get("hub").filter(|h| h.is_object()) {
            Some(h) => {
                let status = if h.get("connected").and_then(Value::as_bool) == Some(true) {
                    format!("{} connected", core.sym.ok)
                } else {
                    format!("{} disconnected", core.sym.err)
                };
                items.push(info(core, "Status", status));
                items.push(info(core, "Transport", text_of(h.get("transport"))));
                items.push(info(core, "Firmware", text_of(h.get("fw"))));
                items.push(info(core, "Board", text_of(h.get("board"))));
                items.push(info(
                    core,
                    "Errors",
                    format!("crc {}, dropped {}", text_of(h.get("crc_errors")), text_of(h.get("dropped"))),
                ));
            }
            None => items.push(info(core, "Status", "not configured")),
        }
        items.push(Item::Header("Rig file".into()));
        let rig = match (s.get("rig_id").and_then(Value::as_str), s.get("calib_version").and_then(Value::as_str)) {
            (Some(r), Some(c)) => format!("rig {r}, calibration {c}"),
            (Some(r), None) => format!("rig {r}"),
            _ => "—".into(),
        };
        items.push(info(core, "rig.json", rig));
    }

    fn clock_items(core: &Core, items: &mut Vec<Item>) {
        let t = &core.theme;
        items.push(Item::Header("Clock sources".into()));
        let Some(c) = core.value(&ResourceId::ClockStatus) else {
            items.push(match core.slot(&ResourceId::ClockStatus).and_then(|s| s.error.clone()) {
                Some(e) => note(core, format!("Could not read the clock: {e}"), true),
                None => note(core, "Reading clock…", false),
            });
            return;
        };
        let lock = |v: Option<&Value>| match v.and_then(Value::as_bool) {
            Some(true) => format!("{} locked", core.sym.ok),
            _ => format!("{} unlocked", core.sym.warn),
        };
        items.push(info(core, "Timebase", text_of(c.get("timebase"))));
        items.push(info(core, "Lock", lock(c.get("locked"))));
        items.push(info(core, "σ", sigma(c.get("sigma_ns").and_then(Value::as_f64))));
        let sources = c.get("sources").and_then(Value::as_array).cloned().unwrap_or_default();
        if sources.is_empty() {
            return;
        }
        items.push(Item::Text(Line::from(Span::styled(
            format!("  {:<12} {:<10} {:<12} {:>12} {:>10} {:>10}", "Source", "Kind", "Lock", "Offset", "Skew", "σ"),
            t.dim(),
        ))));
        for s in sources {
            let locked = s.get("locked").and_then(Value::as_bool) == Some(true);
            let offset = s
                .get("offset_ns")
                .and_then(Value::as_f64)
                .map(|n| format!("{:.3} ms", n / 1e6))
                .unwrap_or_else(|| "—".into());
            let skew = s
                .get("skew_ppm")
                .and_then(Value::as_f64)
                .map(|p| format!("{p:.1} ppm"))
                .unwrap_or_else(|| "—".into());
            items.push(Item::Text(Line::from(vec![
                Span::styled(format!("  {:<12} ", text_of(s.get("id"))), t.text()),
                Span::styled(format!("{:<10} ", text_of(s.get("kind"))), t.dim()),
                Span::styled(
                    format!("{:<12} ", lock(s.get("locked"))),
                    Style::default().fg(if locked { t.success() } else { t.warning() }),
                ),
                Span::styled(format!("{offset:>12} {skew:>10} {:>10}", sigma(s.get("sigma_ns").and_then(Value::as_f64))), t.text()),
            ])));
        }
    }

    fn items(&self, core: &Core) -> Vec<Item> {
        let mut items = Vec::new();
        Self::hub_items(core, &mut items);
        Self::clock_items(core, &mut items);
        items
    }

    fn hint(core: &Core) -> String {
        if core.target.remote {
            "l loads a rig.json from a path on this workstation (not on the agent)".into()
        } else {
            "l loads a rig.json from a path on this machine".into()
        }
    }
}

impl Bespoke for Page {
    fn subs(&self, _core: &Core) -> Vec<Sub> {
        vec![
            Sub::every_ms(ResourceId::SensorsStatus, "/api/v1/sensors", 5000),
            Sub::every_ms(ResourceId::ClockStatus, "/api/v1/clock", 5000),
        ]
    }

    fn keys(&self) -> Option<PageKeys> {
        Some(PageKeys::Sensors)
    }

    fn on_command(&mut self, core: &mut Core, cmd: Command) -> bool {
        let items = self.items(core);
        if kit::nav(&mut self.rows, &items, cmd) {
            return true;
        }
        match cmd {
            Command::LoadRig => {
                let mut input = Overlay::input(Ret::new(ROUTE, "rig_path"), "Load rig file", "Path to rig.json", "");
                input.hint = Some(Self::hint(core));
                core.open(Overlay::Input(input));
            }
            Command::Left | Command::Right | Command::Activate => {}
            _ => return false,
        }
        true
    }

    fn on_overlay(&mut self, core: &mut Core, ret: Ret, result: OverlayResult) {
        let ("rig_path", OverlayResult::Text(path)) = (ret.tag, result) else { return };
        let path = path.trim().to_string();
        match load_rig(Path::new(&path)) {
            Ok(doc) => core.submit(
                Request::new(
                    "PUT",
                    "/api/v1/sensors/rig",
                    ROUTE,
                    Purpose::Page { tag: "rig", data: json!(path) },
                )
                .body(doc),
            ),
            Err(why) => core.toast(ToastKind::Error, why),
        }
    }

    fn on_reply(&mut self, core: &mut Core, reply: Reply) {
        let Purpose::Page { tag: "rig", .. } = &reply.purpose else { return };
        match &reply.result {
            Ok(body) => {
                let rig = text_of(body.get("rig_id"));
                let calib = text_of(body.get("calib_version"));
                core.toast(ToastKind::Success, format!("Rig {rig} saved (calibration {calib})."));
            }
            Err(e) if error_code(e).as_deref() == Some("E_RIG_INVALID") => {
                core.open(Overlay::result(
                    "Rig file rejected",
                    vec![(ToastKind::Error, format!("Rejected (E_RIG_INVALID): {}", detail_text(e)))],
                ));
            }
            Err(e) => core.toast(ToastKind::Error, format!("Rig upload: {}", detail_text(e))),
        }
        core.fetch_now(ResourceId::SensorsStatus);
    }

    fn on_click(&mut self, core: &mut Core, hit: &Hit) {
        let items = self.items(core);
        kit::click(&mut self.rows, &items, hit);
    }

    fn render(&mut self, f: &mut Frame, area: Rect, core: &Core, out: &mut FrameOut) {
        let items = self.items(core);
        let prefix = rowkit::prefix(BespokeId::Sensors.id());
        rows_view::render(f, area, core, &items, &mut self.rows, &prefix, true, out);
    }

    fn context_line(&self, core: &Core) -> Option<String> {
        Some(Self::hint(core))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str, body: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ados-tui-rig-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn rig_file_loads_json_and_rejects_the_rest() {
        let ok = temp("rig.json", r#"{"rig_id":"visor-01","sensors":[]}"#);
        assert_eq!(load_rig(&ok).unwrap()["rig_id"], "visor-01");
        let bad = temp("bad.json", "rig_id = visor");
        assert!(load_rig(&bad).unwrap_err().starts_with("Not valid JSON"));
        let arr = temp("arr.json", "[1,2]");
        assert_eq!(load_rig(&arr).unwrap_err(), "The rig file must be a JSON object.");
        let missing = ok.with_file_name("absent.json");
        assert!(load_rig(&missing).unwrap_err().starts_with("Could not read"));
        let _ = std::fs::remove_dir_all(ok.parent().unwrap());
    }

    #[test]
    fn sigma_formats() {
        assert_eq!(sigma(Some(40_000.0)), "40 µs");
        assert_eq!(sigma(Some(2_500_000.0)), "2.50 ms");
        assert_eq!(sigma(Some(-1.0)), "—");
    }
}
