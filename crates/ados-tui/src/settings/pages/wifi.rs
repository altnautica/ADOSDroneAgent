//! Settings page: Wi-Fi client. The live station status, a scan with join,
//! a hidden-network join, disconnect, and the saved profiles with their
//! autoconnect flag.

use ratatui::layout::Rect;
use ratatui::Frame;
use serde_json::{json, Value};

use super::kit::{self, detail_text};
use super::wifi_model::{self as model, Network};
use crate::app::{Core, FrameOut, Hit};
use crate::keymap::{Command, PageKeys};
use crate::overlay::{Overlay, OverlayResult, Ret};
use crate::poll::{ResourceId, Sub};
use crate::settings::page::{Bespoke, BespokeId};
use crate::settings::rows_view::{self, RowsState};
use crate::toast::ToastKind;
use crate::worker::{Purpose, Reply, Request, Route};

pub(super) const ROUTE: Route = Route::page("wifi");

/// A join in progress: the network and the typed passphrase, held only until
/// the join finishes (for the hotspot-busy re-send). Never stored elsewhere.
#[derive(Debug, Clone)]
pub(super) struct Pending {
    pub(super) ssid: String,
    pub(super) passphrase: Option<String>,
}

#[derive(Default)]
pub struct Page {
    pub(super) rows: RowsState,
    pub(super) scanning: bool,
    /// `None` until the first scan finished.
    pub(super) networks: Option<Vec<Network>>,
    pub(super) pending: Option<Pending>,
}

fn id() -> &'static str {
    BespokeId::Wifi.id()
}

fn need_pass(s: &str) -> Result<(), String> {
    if s.is_empty() {
        Err("Enter the passphrase".into())
    } else {
        Ok(())
    }
}

fn need_ssid(s: &str) -> Result<(), String> {
    if s.trim().is_empty() {
        Err("Enter the network name".into())
    } else {
        Ok(())
    }
}

impl Bespoke for Page {
    fn subs(&self, _core: &Core) -> Vec<Sub> {
        vec![
            Sub::every_ms(
                ResourceId::WifiStatus,
                "/api/v1/network/client/status",
                5000,
            ),
            Sub::every_ms(
                ResourceId::WifiConfigured,
                "/api/v1/network/client/configured",
                15000,
            ),
        ]
    }

    fn keys(&self) -> Option<PageKeys> {
        Some(PageKeys::Wifi)
    }

    fn on_command(&mut self, core: &mut Core, cmd: Command) -> bool {
        let items = self.items(core);
        if kit::nav(&mut self.rows, &items, cmd) {
            return true;
        }
        match cmd {
            Command::Scan => self.scan(core),
            Command::Activate => {
                // Enter on a saved network flips autoconnect, like ←/→.
                if self.toggle_autoconnect(core) {
                    return true;
                }
                let Some(n) = self.focused_network(core) else {
                    return true;
                };
                if !core.guard_disarmed() {
                    return true;
                }
                if n.secured() {
                    let mut input = Overlay::input(
                        Ret::new(ROUTE, "pass").with(json!({ "ssid": n.ssid })),
                        format!("Join {}", n.ssid),
                        format!("Passphrase ({})", n.security),
                        "",
                    );
                    input.masked = true;
                    input.validate = Some(need_pass);
                    core.open(Overlay::Input(input));
                } else {
                    self.confirm_join(core, n.ssid, None);
                }
            }
            Command::Hidden => {
                if !core.guard_disarmed() {
                    return true;
                }
                let mut input = Overlay::input(
                    Ret::new(ROUTE, "hidden_ssid"),
                    "Hidden network",
                    "Network name (SSID)",
                    "",
                );
                input.validate = Some(need_ssid);
                core.open(Overlay::Input(input));
            }
            Command::Disconnect => {
                if !core.guard_disarmed() {
                    return true;
                }
                let ssid = core
                    .value(&ResourceId::WifiStatus)
                    .and_then(|v| v.get("ssid"))
                    .and_then(Value::as_str)
                    .map(|s| format!("Disconnect from {s}?"))
                    .unwrap_or_else(|| "Disconnect the Wi-Fi client link?".into());
                core.open(Overlay::confirm(
                    Ret::new(ROUTE, "leave"),
                    "Disconnect",
                    vec![ssid],
                ));
            }
            Command::Forget => {
                let Some(s) = self.focused_saved(core) else {
                    return true;
                };
                if !core.guard_disarmed() {
                    return true;
                }
                core.open(Overlay::confirm(
                    Ret::new(ROUTE, "forget").with(json!(s.name)),
                    "Forget network",
                    vec![format!(
                        "Forget {}? Its saved passphrase is deleted.",
                        s.name
                    )],
                ));
            }
            Command::Left | Command::Right => {
                self.toggle_autoconnect(core);
            }
            Command::Copy => {
                if let Some(n) = self.focused_network(core) {
                    core.copy(&n.ssid);
                } else if let Some(s) = self.focused_saved(core) {
                    core.copy(&s.name);
                }
            }
            _ => return false,
        }
        true
    }

    fn on_overlay(&mut self, core: &mut Core, ret: Ret, result: OverlayResult) {
        let ssid_of = |ret: &Ret| {
            ret.data
                .get("ssid")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        match (ret.tag, result) {
            ("pass", OverlayResult::Text(pass)) => {
                self.confirm_join(core, ssid_of(&ret), Some(pass))
            }
            ("hidden_ssid", OverlayResult::Text(ssid)) => {
                let ssid = ssid.trim().to_string();
                let mut input = Overlay::input(
                    Ret::new(ROUTE, "hidden_pass").with(json!({ "ssid": ssid })),
                    format!("Join {ssid}"),
                    "Passphrase",
                    "",
                );
                input.masked = true;
                input.hint = Some("Leave empty for an open network".into());
                core.open(Overlay::Input(input));
            }
            ("hidden_pass", OverlayResult::Text(pass)) => {
                let pass = (!pass.is_empty()).then_some(pass);
                self.confirm_join(core, ssid_of(&ret), pass);
            }
            ("join", OverlayResult::Confirmed) => {
                if core.guard_disarmed() {
                    self.send_join(core, false);
                } else {
                    self.pending = None;
                }
            }
            ("force", OverlayResult::Confirmed) => {
                if core.guard_disarmed() {
                    self.send_join(core, true);
                } else {
                    self.pending = None;
                }
            }
            ("join" | "force", _) => self.pending = None,
            ("leave", OverlayResult::Confirmed) => {
                if core.guard_disarmed() {
                    core.submit(Request::new(
                        "DELETE",
                        "/api/v1/network/client",
                        ROUTE,
                        Purpose::Page {
                            tag: "leave",
                            data: Value::Null,
                        },
                    ));
                }
            }
            ("forget", OverlayResult::Confirmed) => {
                let name = ret.data.as_str().unwrap_or_default().to_string();
                if core.guard_disarmed() {
                    core.submit(Request::new(
                        "DELETE",
                        format!(
                            "/api/v1/network/client/configured/{}",
                            crate::api_path::seg(&name)
                        ),
                        ROUTE,
                        Purpose::Page {
                            tag: "forget",
                            data: json!(name),
                        },
                    ));
                }
            }
            _ => {}
        }
    }

    fn on_reply(&mut self, core: &mut Core, reply: Reply) {
        match &reply.purpose {
            Purpose::WifiScan => {
                self.scanning = false;
                match &reply.result {
                    Ok(body) => self.networks = Some(model::parse_scan(body)),
                    Err(e) => {
                        core.toast(ToastKind::Error, format!("Scan failed: {}", detail_text(e)))
                    }
                }
            }
            Purpose::WifiJoin { ssid, forced } => {
                let (ssid, forced) = (ssid.clone(), *forced);
                self.join_reply(core, &ssid, forced, &reply.result);
            }
            Purpose::Page { tag: "leave", .. } => {
                match &reply.result {
                    Ok(body) => {
                        let msg = match body.get("previous_ssid").and_then(Value::as_str) {
                            Some(s) if !s.is_empty() => format!("Disconnected from {s}"),
                            _ => "Disconnected".to_string(),
                        };
                        core.toast(ToastKind::Success, msg);
                    }
                    Err(e) => {
                        core.toast(ToastKind::Error, format!("Disconnect: {}", detail_text(e)))
                    }
                }
                core.fetch_now(ResourceId::WifiStatus);
            }
            Purpose::Page {
                tag: "forget",
                data,
            } => {
                let name = data.as_str().unwrap_or_default();
                match &reply.result {
                    Ok(_) => core.toast(ToastKind::Success, format!("Forgot {name}")),
                    Err(e) => core.toast(
                        ToastKind::Error,
                        format!("Forget {name}: {}", detail_text(e)),
                    ),
                }
                core.fetch_now(ResourceId::WifiConfigured);
            }
            Purpose::Page {
                tag: "autoconnect",
                data,
            } => {
                let name = data.get("name").and_then(Value::as_str).unwrap_or_default();
                let row = format!("{}saved:{name}", kit::prefix(id()));
                match &reply.result {
                    Ok(_) => {
                        let on = data.get("enabled").and_then(Value::as_bool) == Some(true);
                        let state = if on { "on" } else { "off" };
                        core.toast(ToastKind::Success, format!("{name} autoconnect {state}"));
                        core.flash(row, true);
                    }
                    Err(e) => {
                        core.toast(ToastKind::Error, format!("{name}: {}", detail_text(e)));
                        core.flash(row, false);
                    }
                }
                core.fetch_now(ResourceId::WifiConfigured);
            }
            _ => {}
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
            &kit::prefix(id()),
            true,
            out,
        );
    }

    fn context_line(&self, core: &Core) -> Option<String> {
        if let Some(n) = self.focused_network(core) {
            let sec = if n.secured() {
                n.security.as_str()
            } else {
                "open"
            };
            return Some(format!(
                "{} · signal {} % · {sec} · Enter joins",
                n.ssid, n.signal
            ));
        }
        let items = self.items(core);
        if let Some(r) = self.rows.focused_row(&items) {
            return Some(r.help.clone());
        }
        Some("s scans for networks (up to 15 s) · h joins a hidden network · D disconnects".into())
    }

    fn animating(&self, _core: &Core) -> bool {
        self.scanning
    }
}
