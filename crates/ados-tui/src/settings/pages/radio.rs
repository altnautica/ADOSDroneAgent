//! Settings page: the WFB radio. Channel (a coordinated hop) and runtime TX
//! power, read from `/api/wfb`; the agent's validation text is shown as is.

use ratatui::layout::Rect;
use ratatui::Frame;
use serde_json::{json, Value};

use super::kit::{self, detail_text, text_of};
use crate::app::{Core, FrameOut, Hit};
use crate::keymap::Command;
use crate::overlay::{OverlayResult, Ret};
use crate::poll::{ResourceId, Sub};
use crate::settings::page::{Bespoke, BespokeId};
use crate::settings::row;
use crate::settings::rows_view::{self, info, note, Item, RowsState};
use crate::settings::schema::Kind;
use crate::toast::ToastKind;
use crate::worker::{Purpose, Reply, Request, Route};

const ROUTE: Route = Route::page("radio");

const CHANNEL_HINT: &str = "The WFB home channel. A change is a coordinated hop: the ground station is told first, then both sides move.";
const TX_HINT: &str = "Runtime transmit power in dBm, bounded by the configured ceiling. Saved so it survives a restart.";

#[derive(Default)]
pub struct Page {
    rows: RowsState,
}

/// The agent's TX-power floor: it refuses anything below 1 dBm.
pub const TX_POWER_FLOOR_DBM: i64 = 1;

/// The TX-power row kind: the agent's floor up to the radio's ceiling.
pub fn tx_power_kind(max: Option<i64>) -> Kind {
    Kind::Int {
        min: Some(TX_POWER_FLOOR_DBM),
        max,
    }
}

/// Parse a typed TX power against the ceiling.
pub fn parse_tx_power(text: &str, max: Option<i64>) -> Result<i64, String> {
    row::parse(&tx_power_kind(max), text).map(|v| v.as_i64().unwrap_or_default())
}

impl Page {
    fn tx_max(core: &Core) -> Option<i64> {
        core.value(&ResourceId::Wfb)
            .and_then(|w| w.get("tx_power_max_dbm"))
            .and_then(Value::as_i64)
    }

    fn items(&self, core: &Core) -> Vec<Item> {
        let mut items = vec![Item::Header("Radio".into())];
        let Some(w) = core.value(&ResourceId::Wfb) else {
            items.push(
                match core.slot(&ResourceId::Wfb).and_then(|s| s.error.clone()) {
                    Some(e) => note(core, format!("Could not read the radio: {e}"), true),
                    None => note(core, "Reading radio…", false),
                },
            );
            return items;
        };
        items.push(Item::Row(kit::row(
            "channel",
            "Channel",
            CHANNEL_HINT,
            Kind::Int {
                min: Some(1),
                max: None,
            },
            w.get("channel").cloned(),
        )));
        items.push(Item::Row(kit::row(
            "tx_power",
            "TX power dBm",
            TX_HINT,
            tx_power_kind(Self::tx_max(core)),
            w.get("tx_power_dbm").cloned(),
        )));
        items.push(info(core, "Live channel", text_of(w.get("actual_channel"))));
        let freq = w
            .get("frequency_mhz")
            .and_then(Value::as_f64)
            .map(|f| format!("{f:.0} MHz"))
            .unwrap_or_else(|| "—".into());
        items.push(info(core, "Frequency", freq));
        items.push(info(
            core,
            "TX power ceiling",
            format!("{} dBm", text_of(w.get("tx_power_max_dbm"))),
        ));
        items.push(info(
            core,
            "Regulatory domain",
            text_of(w.get("regulatory_domain")),
        ));
        items.push(info(core, "State", text_of(w.get("state"))));
        items
    }

    fn write(core: &mut Core, key: &str, value: i64) {
        let req = if key == "channel" {
            Request::new(
                "POST",
                "/api/wfb/channel",
                ROUTE,
                Purpose::Page {
                    tag: "channel",
                    data: json!(value),
                },
            )
            .body(json!({ "channel": value }))
        } else {
            Request::new(
                "PUT",
                "/api/wfb/tx-power",
                ROUTE,
                Purpose::Page {
                    tag: "tx_power",
                    data: json!(value),
                },
            )
            .body(json!({ "tx_power_dbm": value }))
        };
        core.submit(req);
    }
}

impl Bespoke for Page {
    fn subs(&self, _core: &Core) -> Vec<Sub> {
        vec![Sub::every_ms(ResourceId::Wfb, "/api/wfb", 2000)]
    }

    fn on_command(&mut self, core: &mut Core, cmd: Command) -> bool {
        let items = self.items(core);
        if kit::nav(&mut self.rows, &items, cmd) {
            return true;
        }
        match cmd {
            Command::Activate => {
                if let Some(row) = self.rows.focused_row(&items).cloned() {
                    kit::open_editor(core, ROUTE, &row);
                }
            }
            Command::Copy => {
                if let Some(row) = self.rows.focused_row(&items) {
                    let text = row::display(&row.kind, row.value.as_ref(), false, true);
                    core.copy(&text);
                }
            }
            Command::Left | Command::Right => {}
            _ => return false,
        }
        true
    }

    fn on_overlay(&mut self, core: &mut Core, ret: Ret, result: OverlayResult) {
        if ret.tag != kit::CONFIG_EDIT {
            return;
        }
        let key = ret.data.as_str().unwrap_or_default().to_string();
        let OverlayResult::Text(text) = result else {
            return;
        };
        let parsed = if key == "tx_power" {
            parse_tx_power(&text, Self::tx_max(core))
        } else {
            row::parse(
                &Kind::Int {
                    min: Some(1),
                    max: None,
                },
                &text,
            )
            .map(|v| v.as_i64().unwrap_or_default())
        };
        match parsed {
            Ok(v) => Self::write(core, &key, v),
            Err(why) => core.toast(ToastKind::Error, why),
        }
    }

    fn on_reply(&mut self, core: &mut Core, reply: Reply) {
        let Purpose::Page { tag, data } = &reply.purpose else {
            return;
        };
        let row = format!("{}{tag}", kit::prefix(BespokeId::Radio.id()));
        match (&reply.result, *tag) {
            (Ok(body), "channel") => {
                let target = body
                    .get("target_channel")
                    .cloned()
                    .unwrap_or_else(|| data.clone());
                core.toast(
                    ToastKind::Success,
                    format!("Channel hop to {target} initiated"),
                );
                core.flash(row, true);
            }
            (Ok(body), "tx_power") => {
                let eff = body
                    .get("effective_dbm")
                    .cloned()
                    .unwrap_or_else(|| data.clone());
                if body.get("persisted").and_then(Value::as_bool) == Some(false) {
                    let why = text_of(body.get("persist_error"));
                    core.toast(
                        ToastKind::Warn,
                        format!("TX power {eff} dBm applied but not persisted: {why}"),
                    );
                } else {
                    core.toast(ToastKind::Success, format!("TX power {eff} dBm saved"));
                }
                core.flash(row, true);
            }
            (Err(e), "channel" | "tx_power") => {
                let label = if *tag == "channel" {
                    "Channel"
                } else {
                    "TX power"
                };
                core.toast(ToastKind::Error, format!("{label}: {}", detail_text(e)));
                core.flash(row, false);
            }
            _ => return,
        }
        core.fetch_now(ResourceId::Wfb);
    }

    fn on_click(&mut self, core: &mut Core, hit: &Hit) {
        let items = self.items(core);
        kit::click(&mut self.rows, &items, hit);
    }

    fn render(&mut self, f: &mut Frame, area: Rect, core: &Core, out: &mut FrameOut) {
        let items = self.items(core);
        let prefix = kit::prefix(BespokeId::Radio.id());
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
    fn tx_power_is_bounded_by_the_ceiling() {
        assert_eq!(parse_tx_power("12", Some(15)), Ok(12));
        assert_eq!(parse_tx_power("1", Some(15)), Ok(1));
        assert_eq!(parse_tx_power("16", Some(15)), Err("Maximum is 15".into()));
        assert_eq!(
            parse_tx_power("0", Some(15)),
            Err("Minimum is 1".into()),
            "the agent refuses 0 dBm"
        );
        assert!(parse_tx_power("loud", Some(15)).is_err());
        assert_eq!(parse_tx_power("30", None), Ok(30));
    }
}
