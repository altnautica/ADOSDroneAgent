//! Settings page: cellular. A ground station shows its modem block and
//! writes the modem config sidecar; every other profile edits the
//! `network.cellular.*` config keys.

use ratatui::layout::Rect;
use ratatui::Frame;
use serde_json::{json, Value};

use super::kit::{self, detail_text, text_of};
use crate::app::{Core, FrameOut, Hit};
use crate::keymap::Command;
use crate::overlay::{OverlayResult, Ret};
use crate::poll::{ResourceId, Sub};
use crate::settings::config_rows;
use crate::settings::page::{Bespoke, BespokeId};
use crate::settings::row;
use crate::settings::rows_view::{self, info, note, Item, RowsState};
use crate::settings::schema::Kind;
use crate::toast::ToastKind;
use crate::worker::{Purpose, Reply, Request, Route};

const ROUTE: Route = Route::page("cellular");

const ENABLED_HINT_GS: &str = "Bring the modem up as an uplink leg. It only carries traffic when it is higher in the failover order than a connected wired or Wi-Fi link.";
const ENABLED_HINT: &str = "Enable an attached USB cellular modem as an uplink.";
const APN_HINT: &str = "The carrier access point name for your SIM (e.g. internet).";
const CAP_HINT: &str = "Warn once the modem has used this much data. 0 disables the cap.";
const NO_MM_NOTE: &str = "This node has no live modem manager, so signal and usage are not reported here. You can still configure the cellular uplink keys below; a ground-station node shows live modem status.";

#[derive(Default)]
pub struct Page {
    rows: RowsState,
}

fn is_gs(core: &Core) -> bool {
    core.profile() == "ground_station"
}

/// The data cap in GB from the modem view's `cap_mb` (null = no cap).
fn cap_gb(modem: &Value) -> Option<Value> {
    match modem.get("cap_mb").and_then(Value::as_f64) {
        Some(mb) => Some(json!(((mb / 1024.0) * 100.0).round() / 100.0)),
        None => Some(json!(0)),
    }
}

impl Page {
    fn gs_items(&self, core: &Core) -> Vec<Item> {
        let mut items = vec![Item::Header("Modem".into())];
        match core.value(&ResourceId::ModemStatus) {
            Some(s) => {
                let presence = match (
                    s.get("present").and_then(Value::as_bool),
                    s.get("reason").and_then(Value::as_str),
                ) {
                    (Some(true), _) => "detected",
                    (Some(false), Some("modemmanager_not_installed")) => {
                        "ModemManager not installed"
                    }
                    (Some(false), _) => "no modem detected",
                    (None, _) => "unknown",
                };
                items.push(info(core, "Presence", presence));
            }
            None => items.push(
                match core
                    .slot(&ResourceId::ModemStatus)
                    .and_then(|s| s.error.clone())
                {
                    Some(e) => note(
                        core,
                        format!("Could not read the modem from this node: {e}"),
                        true,
                    ),
                    None => note(core, "Reading modem…", false),
                },
            ),
        }
        let modem = core.value(&ResourceId::ModemConfig);
        if let Some(m) = modem {
            for (label, key) in [
                ("State", "state"),
                ("Operator", "operator"),
                ("Technology", "technology"),
                ("Signal", "signal_quality"),
                ("IP", "ip"),
            ] {
                items.push(info(core, label, text_of(m.get(key))));
            }
            let used = match (
                m.get("data_used_mb").and_then(Value::as_f64),
                m.get("cap_mb").and_then(Value::as_f64),
            ) {
                (Some(u), Some(c)) => format!("{u:.0} / {c:.0} MB ({})", text_of(m.get("percent"))),
                (Some(u), None) => format!("{u:.0} MB"),
                _ => "—".into(),
            };
            items.push(info(core, "Data used", used));
        }
        items.push(Item::Header("Cellular uplink".into()));
        match modem {
            Some(m) => {
                items.push(Item::Row(kit::row(
                    "enabled",
                    "Enabled",
                    ENABLED_HINT_GS,
                    Kind::Bool,
                    m.get("enabled").cloned(),
                )));
                items.push(Item::Row(kit::row(
                    "apn",
                    "APN",
                    APN_HINT,
                    Kind::Str { pattern: None },
                    Some(m.get("apn").cloned().unwrap_or(Value::Null)),
                )));
                items.push(Item::Row(kit::row(
                    "cap_gb",
                    "Data cap (GB)",
                    CAP_HINT,
                    Kind::Num {
                        min: Some(0.0),
                        max: None,
                    },
                    cap_gb(m),
                )));
            }
            None => items.push(
                match core
                    .slot(&ResourceId::ModemConfig)
                    .and_then(|s| s.error.clone())
                {
                    Some(e) => note(
                        core,
                        format!("Could not read the cellular config from this node: {e}"),
                        true,
                    ),
                    None => note(core, "Reading config…", false),
                },
            ),
        }
        items
    }

    fn config_items(&self, core: &Core) -> Vec<Item> {
        let mut items = vec![
            Item::Header("Cellular uplink".into()),
            note(core, NO_MM_NOTE, false),
        ];
        if let Some(n) = config_rows::status_note(core) {
            items.push(n);
        }
        items.push(Item::Row(config_rows::config_row(
            core,
            "network.cellular.enabled",
            "Enabled",
            ENABLED_HINT,
        )));
        items.push(Item::Row(config_rows::config_row(
            core,
            "network.cellular.apn",
            "APN",
            APN_HINT,
        )));
        items
    }

    fn items(&self, core: &Core) -> Vec<Item> {
        if is_gs(core) {
            self.gs_items(core)
        } else {
            self.config_items(core)
        }
    }

    /// Write one modem field through the ground-station modem route.
    fn put_modem(core: &mut Core, field: &str, value: Value) {
        core.submit(
            Request::new(
                "PUT",
                "/api/v1/ground-station/network/modem",
                ROUTE,
                Purpose::Page {
                    tag: "modem",
                    data: json!({ "field": field, "value": value }),
                },
            )
            .body(json!({ field: value })),
        );
    }
}

fn saved_text(field: &str, value: &Value) -> String {
    match field {
        "enabled" if value.as_bool() == Some(true) => "Modem enabled.".into(),
        "enabled" => "Modem disabled.".into(),
        "apn" => "APN saved.".into(),
        _ if value.as_f64() == Some(0.0) => "Data cap cleared.".into(),
        _ => "Data cap saved.".into(),
    }
}

impl Bespoke for Page {
    fn subs(&self, core: &Core) -> Vec<Sub> {
        if is_gs(core) {
            vec![
                Sub::every_ms(
                    ResourceId::ModemStatus,
                    "/api/v1/ground-station/modem-status",
                    5000,
                ),
                Sub::every_ms(
                    ResourceId::ModemConfig,
                    "/api/v1/ground-station/network/modem",
                    5000,
                ),
            ]
        } else {
            config_rows::subs(core)
        }
    }

    fn on_command(&mut self, core: &mut Core, cmd: Command) -> bool {
        let items = self.items(core);
        if kit::nav(&mut self.rows, &items, cmd) {
            return true;
        }
        let Some(row) = self.rows.focused_row(&items).cloned() else {
            return matches!(cmd, Command::Left | Command::Right | Command::Activate);
        };
        let page = BespokeId::Cellular.id();
        match cmd {
            Command::Left | Command::Right if is_gs(core) => {
                if let Some(next) = row::cycle(&row.kind, row.value.as_ref(), cmd == Command::Right)
                {
                    Self::put_modem(core, &row.key, next);
                }
            }
            Command::Left | Command::Right => {
                kit::config_cycle(core, ROUTE, page, &row, cmd == Command::Right);
            }
            Command::Activate if is_gs(core) => {
                if row.kind == Kind::Bool {
                    if let Some(next) = row::cycle(&row.kind, row.value.as_ref(), true) {
                        Self::put_modem(core, &row.key, next);
                    }
                } else {
                    kit::open_editor(core, ROUTE, &row);
                }
            }
            Command::Activate => kit::config_edit(core, ROUTE, page, &row),
            Command::Copy => core.copy(&row::display(&row.kind, row.value.as_ref(), false, true)),
            _ => return false,
        }
        true
    }

    fn on_overlay(&mut self, core: &mut Core, ret: Ret, result: OverlayResult) {
        if ret.tag != kit::CONFIG_EDIT {
            return;
        }
        let key = ret.data.as_str().unwrap_or_default();
        let items = self.items(core);
        let Some(row) = kit::find_row(&items, key).cloned() else {
            return;
        };
        if is_gs(core) {
            match row::value_from_result(&row.kind, &result) {
                Some(Ok(v)) => Self::put_modem(core, &row.key, v),
                Some(Err(why)) => core.toast(ToastKind::Error, format!("{}: {why}", row.label)),
                None => {}
            }
        } else {
            kit::config_result(core, ROUTE, BespokeId::Cellular.id(), &row, &result);
        }
    }

    fn on_reply(&mut self, core: &mut Core, reply: Reply) {
        let items = self.items(core);
        if kit::config_reply(core, &items, &reply) {
            return;
        }
        if let Purpose::Page { tag: "modem", data } = &reply.purpose {
            let field = data
                .get("field")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let row = format!("{}{field}", kit::prefix(BespokeId::Cellular.id()));
            match &reply.result {
                Ok(_) => {
                    core.toast(ToastKind::Success, saved_text(field, &data["value"]));
                    core.flash(row, true);
                }
                Err(e) => {
                    core.toast(
                        ToastKind::Error,
                        format!("Could not update the modem: {}", detail_text(e)),
                    );
                    core.flash(row, false);
                }
            }
            core.fetch_now(ResourceId::ModemConfig);
        }
    }

    fn on_click(&mut self, core: &mut Core, hit: &Hit) {
        let items = self.items(core);
        kit::click(&mut self.rows, &items, hit);
    }

    fn render(&mut self, f: &mut Frame, area: Rect, core: &Core, out: &mut FrameOut) {
        let items = self.items(core);
        let prefix = kit::prefix(BespokeId::Cellular.id());
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
    fn cap_reads_in_gb_and_toasts_name_the_field() {
        assert_eq!(cap_gb(&json!({"cap_mb": 5120})), Some(json!(5.0)));
        assert_eq!(cap_gb(&json!({"cap_mb": null})), Some(json!(0)));
        assert_eq!(saved_text("cap_gb", &json!(0)), "Data cap cleared.");
        assert_eq!(saved_text("enabled", &json!(true)), "Modem enabled.");
    }
}
