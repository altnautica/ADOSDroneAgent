//! Settings page: stable-MAC pinning. The tracked adapters with their pin
//! verdicts, `p` pin / `x` unpin on the focused adapter, and the two
//! `network.mac_pin.*` config switches.

use ados_protocol::rest::RestError;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::Frame;
use serde_json::{json, Value};

use super::kit::{self, detail_text};
use crate::app::{Core, FrameOut, Hit};
use crate::keymap::{Command, PageKeys};
use crate::overlay::{OverlayResult, Ret};
use crate::poll::{ResourceId, Sub};
use crate::settings::config_rows;
use crate::settings::page::{Bespoke, BespokeId};
use crate::settings::rows_view::{self, note, Item, RowsState};
use crate::toast::ToastKind;
use crate::worker::{Purpose, Reply, Request, Route};

const ROUTE: Route = Route::page("mac_pin");

const AUTO_PIN_HINT: &str = "Detect an onboard adapter with no hardware MAC (it randomizes each boot, churning this node's IP) and pin a stable address. The pin is written for the next boot and never touches the live link.";
const LIVE_HINT: &str = "Let the agent re-tag the live interface to fix the IP this session without a reboot. Off by default because re-tagging drops any connection over that interface.";
const PIN_HINT: &str = "p pins the learned MAC so the address stops moving (applies on the next reboot). x removes the pin; the agent re-pins a known random-MAC adapter unless auto-pinning is off.";

#[derive(Default)]
pub struct Page {
    rows: RowsState,
}

/// One tracked adapter from `GET /api/v1/network/mac/adapters`.
#[derive(Debug, Clone, PartialEq)]
struct Adapter {
    name: String,
    vidpid: Option<String>,
    state: String,
    pinned_mac: Option<String>,
    applied_live: bool,
    deferred: Option<String>,
}

fn adapters(body: &Value) -> Vec<Adapter> {
    let s = |a: &Value, k: &str| a.get(k).and_then(Value::as_str).map(str::to_string);
    body.get("adapters")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|a| {
                    Some(Adapter {
                        name: s(a, "name")?,
                        vidpid: s(a, "vidpid"),
                        state: s(a, "state").unwrap_or_else(|| "unknown".into()),
                        pinned_mac: s(a, "pinnedMac"),
                        applied_live: a
                            .get("appliedLive")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        deferred: s(a, "deferredReason"),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

impl Page {
    fn items(&self, core: &Core) -> Vec<Item> {
        let t = &core.theme;
        let mut items = vec![Item::Header("Tracked adapters".into())];
        match core.value(&ResourceId::MacAdapters) {
            Some(body) => {
                let list = adapters(body);
                if list.is_empty() {
                    items.push(note(core, "No adapters need pinning on this node.", false));
                }
                for a in list {
                    let state_color = match a.state.as_str() {
                        "pinned" | "stable" => t.success(),
                        "deferred" => t.warning(),
                        _ => t.info(),
                    };
                    let mut spans = vec![
                        Span::styled(format!("{:<8} ", a.name), t.text()),
                        Span::styled(
                            format!("{:<10} ", a.vidpid.clone().unwrap_or_else(|| "—".into())),
                            t.dim(),
                        ),
                        Span::styled(format!("{:<9} ", a.state), Style::default().fg(state_color)),
                        Span::styled(
                            format!(
                                "{:<18} ",
                                a.pinned_mac.clone().unwrap_or_else(|| "—".into())
                            ),
                            t.text(),
                        ),
                        Span::styled(if a.applied_live { "live" } else { "next boot" }, t.dim()),
                    ];
                    if let Some(why) = &a.deferred {
                        spans.push(Span::styled(
                            format!("  {} Deferred: {why}", core.sym.warn),
                            Style::default().fg(t.warning()),
                        ));
                    }
                    items.push(Item::Entry {
                        key: format!("ad:{}", a.name),
                        line: Line::from(spans),
                    });
                }
            }
            None => items.push(
                match core
                    .slot(&ResourceId::MacAdapters)
                    .and_then(|s| s.error.clone())
                {
                    Some(e) => note(
                        core,
                        format!("Could not read adapter stability from this node: {e}"),
                        true,
                    ),
                    None => note(core, "Reading adapters…", false),
                },
            ),
        }
        items.push(Item::Header("Pinning".into()));
        if let Some(n) = config_rows::status_note(core) {
            items.push(n);
        }
        items.push(Item::Row(config_rows::config_row(
            core,
            "network.mac_pin.enabled",
            "Auto-pin a stable MAC",
            AUTO_PIN_HINT,
        )));
        items.push(Item::Row(config_rows::config_row(
            core,
            "network.mac_pin.apply_live_allowed",
            "Allow live re-tag",
            LIVE_HINT,
        )));
        items
    }

    fn focused_iface(&self, items: &[Item]) -> Option<String> {
        self.rows
            .focused_key(items)
            .and_then(|k| k.strip_prefix("ad:"))
            .map(str::to_string)
    }
}

impl Bespoke for Page {
    fn subs(&self, core: &Core) -> Vec<Sub> {
        let mut subs = vec![Sub::every_ms(
            ResourceId::MacAdapters,
            "/api/v1/network/mac/adapters",
            10000,
        )];
        subs.extend(config_rows::subs(core));
        subs
    }

    fn keys(&self) -> Option<PageKeys> {
        Some(PageKeys::MacPin)
    }

    fn on_command(&mut self, core: &mut Core, cmd: Command) -> bool {
        let items = self.items(core);
        if kit::nav(&mut self.rows, &items, cmd) {
            return true;
        }
        let page = BespokeId::MacPin.id();
        match cmd {
            Command::Pin | Command::Unpin => {
                let Some(iface) = self.focused_iface(&items) else {
                    core.toast(ToastKind::Info, "Select an adapter first");
                    return true;
                };
                let req = if cmd == Command::Pin {
                    Request::new(
                        "POST",
                        "/api/v1/network/mac/pin",
                        ROUTE,
                        Purpose::Page {
                            tag: "pin",
                            data: json!(iface),
                        },
                    )
                    .body(json!({ "iface": iface }))
                } else {
                    Request::new(
                        "DELETE",
                        format!("/api/v1/network/mac/{}", crate::api_path::seg(&iface)),
                        ROUTE,
                        Purpose::Page {
                            tag: "unpin",
                            data: json!(iface),
                        },
                    )
                };
                core.submit(req);
            }
            Command::Left | Command::Right => {
                if let Some(row) = self.rows.focused_row(&items).cloned() {
                    kit::config_cycle(core, ROUTE, page, &row, cmd == Command::Right);
                }
            }
            Command::Activate => {
                if let Some(row) = self.rows.focused_row(&items).cloned() {
                    kit::config_edit(core, ROUTE, page, &row);
                }
            }
            Command::Copy => {
                if let Some(iface) = self.focused_iface(&items) {
                    core.copy(&iface);
                }
            }
            _ => return false,
        }
        true
    }

    fn on_overlay(&mut self, core: &mut Core, ret: Ret, result: OverlayResult) {
        if ret.tag != kit::CONFIG_EDIT {
            return;
        }
        let items = self.items(core);
        if let Some(row) = kit::find_row(&items, ret.data.as_str().unwrap_or_default()).cloned() {
            kit::config_result(core, ROUTE, BespokeId::MacPin.id(), &row, &result);
        }
    }

    fn on_reply(&mut self, core: &mut Core, reply: Reply) {
        let items = self.items(core);
        if kit::config_reply(core, &items, &reply) {
            return;
        }
        let Purpose::Page { tag, data } = &reply.purpose else {
            return;
        };
        let iface = data.as_str().unwrap_or_default();
        let row = format!("{}ad:{iface}", kit::prefix(BespokeId::MacPin.id()));
        let Some((ok, text)) = pin_outcome(tag, iface, &reply.result) else {
            return;
        };
        core.toast(
            if ok {
                ToastKind::Success
            } else {
                ToastKind::Error
            },
            text,
        );
        core.flash(row, ok);
        core.fetch_now(ResourceId::MacAdapters);
    }

    fn on_click(&mut self, core: &mut Core, hit: &Hit) {
        let items = self.items(core);
        kit::click(&mut self.rows, &items, hit);
    }

    fn render(&mut self, f: &mut Frame, area: Rect, core: &Core, out: &mut FrameOut) {
        let items = self.items(core);
        let prefix = kit::prefix(BespokeId::MacPin.id());
        rows_view::render(f, area, core, &items, &mut self.rows, &prefix, true, out);
    }

    fn context_line(&self, core: &Core) -> Option<String> {
        let items = self.items(core);
        if let Some(r) = self.rows.focused_row(&items) {
            return Some(r.help.clone());
        }
        Some(PIN_HINT.into())
    }
}

/// The toast of a pin (`tag` "pin") or unpin ("unpin") reply: `(ok, text)`.
/// Both routes answer a body-level failure as a 200 with `status: "error"`
/// and a `message` (the unpin when the override or `.link` stayed), so the
/// status field decides, not the HTTP code. `None` for another tag.
fn pin_outcome(
    tag: &str,
    iface: &str,
    result: &Result<Value, RestError>,
) -> Option<(bool, String)> {
    let (done, failed) = match tag {
        "pin" => ("Pinned", "Could not pin the adapter"),
        "unpin" => ("Unpinned", "Could not unpin the adapter"),
        _ => return None,
    };
    let str_at = |b: &Value, k: &str| {
        b.get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    Some(match result {
        Ok(body) if body.get("status").and_then(Value::as_str) == Some("error") => {
            let why = str_at(body, "message")
                .or_else(|| str_at(body, "persist_error"))
                .or_else(|| str_at(body, "link_error"))
                .unwrap_or_else(|| "the agent reported an error".into());
            (false, format!("{failed} {iface}: {why}"))
        }
        Ok(body) => match str_at(body, "note") {
            Some(note) => (true, format!("{done} {iface}. {note}")),
            None => (true, format!("{done} {iface}.")),
        },
        Err(e) => (false, format!("{failed}: {}", detail_text(e))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapters_parse_the_camel_case_view() {
        let list = adapters(&json!({"version":2,"adapters":[
            {"name":"wlan0","vidpid":"0bda:a81a","usbPath":"1-1.2","state":"pinned","appliedLive":true,"pinnedMac":"02:c6:75:83:1a:3e"},
            {"name":"eth0","vidpid":null,"usbPath":null,"state":"deferred","appliedLive":false,"deferredReason":"management interface"}
        ]}));
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].pinned_mac.as_deref(), Some("02:c6:75:83:1a:3e"));
        assert!(list[0].applied_live);
        assert_eq!(list[1].vidpid, None);
        assert_eq!(list[1].deferred.as_deref(), Some("management interface"));
    }

    #[test]
    fn pin_replies_classify_on_the_body_status() {
        let ok = pin_outcome(
            "unpin",
            "wlan0",
            &Ok(json!({"status":"ok","iface":"wlan0","note":"re-pinned unless off"})),
        );
        assert_eq!(
            ok,
            Some((true, "Unpinned wlan0. re-pinned unless off".into()))
        );
        let failed = pin_outcome(
            "unpin",
            "wlan0",
            &Ok(
                json!({"status":"error","iface":"wlan0","persist_error":"read-only fs","message":"the adapter is still pinned for the next boot"}),
            ),
        );
        assert_eq!(
            failed,
            Some((
                false,
                "Could not unpin the adapter wlan0: the adapter is still pinned for the next boot"
                    .into()
            ))
        );
        let refused = pin_outcome(
            "pin",
            "eth9",
            &Err(RestError::Status {
                code: 400,
                body: Some(
                    json!({"detail":{"error":{"code":"E_UNKNOWN_ADAPTER","message":"unknown adapter eth9"}}}),
                ),
            }),
        );
        assert_eq!(
            refused,
            Some((
                false,
                "Could not pin the adapter: unknown adapter eth9".into()
            ))
        );
        assert_eq!(pin_outcome("other", "wlan0", &Ok(json!({}))), None);
    }
}
