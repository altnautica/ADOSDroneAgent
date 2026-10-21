//! Settings page: Wi-Fi client. The live station status, a scan with join,
//! a hidden-network join, disconnect, and the saved profiles with their
//! autoconnect flag.

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::Frame;
use serde_json::{json, Value};

use super::kit::{self, detail_text, text_of};
use super::wifi_model::{self as model, JoinConflict, Network, Saved};
use crate::app::{Core, FrameOut, Hit, ARMED_REFUSAL};
use crate::keymap::{Command, PageKeys};
use crate::overlay::{Overlay, OverlayResult, Ret};
use crate::poll::{ResourceId, Sub};
use crate::settings::page::{Bespoke, BespokeId};
use crate::settings::rows_view::{self, info, note, Item, RowsState};
use crate::settings::schema::Kind;
use crate::toast::ToastKind;
use crate::worker::{self, Purpose, Reply, Request, Route};

const ROUTE: Route = Route::page("wifi");

/// A join in progress: the network and the typed passphrase, held only until
/// the join finishes (for the hotspot-busy re-send). Never stored elsewhere.
#[derive(Debug, Clone)]
struct Pending {
    ssid: String,
    passphrase: Option<String>,
}

#[derive(Default)]
pub struct Page {
    rows: RowsState,
    scanning: bool,
    /// `None` until the first scan finished.
    networks: Option<Vec<Network>>,
    pending: Option<Pending>,
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

impl Page {
    fn saved(core: &Core) -> Vec<Saved> {
        core.value(&ResourceId::WifiConfigured)
            .map(model::parse_saved)
            .unwrap_or_default()
    }

    fn items(&self, core: &Core) -> Vec<Item> {
        let t = &core.theme;
        let mut items = vec![Item::Header("Status".into())];
        let slot = core.slot(&ResourceId::WifiStatus);
        match slot {
            Some(s) if s.value.is_none() && model::status_unavailable(s.code, s.error_body.as_ref()) => {
                items.push(note(core, "Wi-Fi control unavailable on this node", true));
            }
            Some(s) if s.value.is_some() => {
                let v = s.value.as_ref().unwrap_or(&Value::Null);
                let connected = v.get("connected").and_then(Value::as_bool);
                let state = match connected {
                    Some(true) => format!("{} connected", core.sym.ok),
                    Some(false) => format!("{} not connected", core.sym.info),
                    None => "unknown".into(),
                };
                items.push(info(core, "Connection", state));
                items.push(info(core, "SSID", text_of(v.get("ssid"))));
                let signal = v
                    .get("signal")
                    .and_then(Value::as_f64)
                    .map(|s| format!("{} {s:.0} %", core.sym.signal_bars(s.clamp(0.0, 100.0) as u8)))
                    .unwrap_or_else(|| "—".into());
                items.push(info(core, "Signal", signal));
                items.push(info(core, "IP", text_of(v.get("ip"))));
                items.push(info(core, "Gateway", text_of(v.get("gateway"))));
                items.push(info(core, "Security", text_of(v.get("security"))));
                if let Some(err) = &s.error {
                    items.push(note(core, format!("Last read failed: {err}"), true));
                }
            }
            Some(s) => items.push(note(core, s.error.clone().unwrap_or_default(), true)),
            None => items.push(note(core, "Reading Wi-Fi status…", false)),
        }

        items.push(Item::Header("Nearby networks".into()));
        if self.scanning {
            items.push(Item::Text(Line::from(vec![
                Span::raw("  "),
                Span::styled(core.sym.spinner_frame(core.frame as usize), t.accented()),
                Span::styled(" Scanning… (up to 15 s)", t.dim()),
            ])));
        }
        match &self.networks {
            None if !self.scanning => items.push(note(core, "Press s to scan for networks", false)),
            Some(nets) if nets.is_empty() && !self.scanning => {
                items.push(note(core, "No networks found", false))
            }
            Some(nets) => {
                let w = nets.iter().map(|n| n.ssid.chars().count()).max().unwrap_or(0).min(32);
                for n in nets {
                    let [bars, ssid, lock, sec, used] = model::network_cells(core.sym, n);
                    let line = Line::from(vec![
                        Span::styled(format!("{bars:<4} "), t.accented()),
                        Span::styled(format!("{ssid:<w$} "), t.text()),
                        Span::styled(format!("{lock:<2} "), t.text()),
                        Span::styled(format!("{sec:<5} "), t.dim()),
                        Span::styled(used, Style::default().fg(t.success())),
                    ]);
                    items.push(Item::Entry {
                        key: format!("net:{}", n.ssid),
                        line,
                    });
                }
            }
            None => {}
        }

        items.push(Item::Header("Saved networks".into()));
        let saved = Self::saved(core);
        if saved.is_empty() {
            let text = match core.slot(&ResourceId::WifiConfigured) {
                None => "Reading saved networks…",
                Some(_) => "No saved networks",
            };
            items.push(note(core, text, false));
        }
        for s in saved {
            items.push(Item::Row(kit::row(
                &format!("saved:{}", s.name),
                &s.name,
                "Autoconnect: rejoin this network automatically when it is in range. ←/→ or Enter changes it, x forgets the network.",
                Kind::Bool,
                Some(json!(s.autoconnect)),
            )));
        }
        items
    }

    /// Flip the focused saved network's autoconnect. False when no saved
    /// network has focus.
    fn toggle_autoconnect(&self, core: &mut Core) -> bool {
        let Some(s) = self.focused_saved(core) else { return false };
        let enabled = !s.autoconnect;
        core.submit(
            Request::new(
                "PUT",
                format!("/api/v1/network/client/configured/{}/autoconnect", crate::api_path::seg(&s.name)),
                ROUTE,
                Purpose::Page {
                    tag: "autoconnect",
                    data: json!({ "name": s.name, "enabled": enabled }),
                },
            )
            .body(json!({ "enabled": enabled })),
        );
        true
    }

    fn scan(&mut self, core: &mut Core) {
        if self.scanning {
            return;
        }
        self.scanning = true;
        core.submit(
            Request::new("GET", "/api/v1/network/client/scan", ROUTE, Purpose::WifiScan)
                .timeout(worker::SCAN_TIMEOUT),
        );
    }

    fn confirm_join(&mut self, core: &mut Core, ssid: String, passphrase: Option<String>) {
        let mut body = vec![format!("Join {ssid}?")];
        if core.target.remote {
            body.push("This session may disconnect if the agent changes networks.".into());
        }
        self.pending = Some(Pending { ssid, passphrase });
        core.open(Overlay::confirm(Ret::new(ROUTE, "join"), "Join network", body));
    }

    fn send_join(&self, core: &mut Core, forced: bool) {
        let Some(p) = &self.pending else { return };
        core.submit(
            Request::new(
                "PUT",
                "/api/v1/network/client/join",
                ROUTE,
                Purpose::WifiJoin {
                    ssid: p.ssid.clone(),
                    forced,
                },
            )
            .body(model::join_body(&p.ssid, p.passphrase.as_deref(), forced)),
        );
    }

    fn join_reply(&mut self, core: &mut Core, ssid: &str, forced: bool, result: &Result<Value, ados_protocol::rest::RestError>) {
        match result {
            Ok(body) => {
                self.pending = None;
                if body.get("joined").and_then(Value::as_bool) == Some(true) {
                    let msg = match body.get("ip").and_then(Value::as_str) {
                        Some(ip) => format!("Joined {ssid}, ip {ip}"),
                        None => format!("Joined {ssid}"),
                    };
                    core.toast(ToastKind::Success, msg);
                } else {
                    let why = text_of(body.get("error"));
                    core.toast(ToastKind::Error, format!("Join {ssid} failed: {why}"));
                }
                core.fetch_now(ResourceId::WifiStatus);
                core.fetch_now(ResourceId::WifiConfigured);
            }
            Err(e) => match model::join_conflict(e) {
                Some(JoinConflict::HotspotBusy) if !forced => {
                    core.open(Overlay::confirm(
                        Ret::new(ROUTE, "force"),
                        "Hotspot busy",
                        vec!["The hotspot is serving clients. Join anyway?".into()],
                    ));
                }
                Some(JoinConflict::Armed) => {
                    self.pending = None;
                    core.toast(ToastKind::Error, ARMED_REFUSAL);
                }
                _ => {
                    self.pending = None;
                    core.toast(ToastKind::Error, format!("Join {ssid}: {}", detail_text(e)));
                }
            },
        }
    }

    fn focused_network(&self, core: &Core) -> Option<Network> {
        let items = self.items(core);
        let key = self.rows.focused_key(&items)?.strip_prefix("net:")?.to_string();
        self.networks.as_ref()?.iter().find(|n| n.ssid == key).cloned()
    }

    fn focused_saved(&self, core: &Core) -> Option<Saved> {
        let items = self.items(core);
        let name = self.rows.focused_key(&items)?.strip_prefix("saved:")?.to_string();
        Self::saved(core).into_iter().find(|s| s.name == name)
    }
}

impl Bespoke for Page {
    fn subs(&self, _core: &Core) -> Vec<Sub> {
        vec![
            Sub::every_ms(ResourceId::WifiStatus, "/api/v1/network/client/status", 5000),
            Sub::every_ms(ResourceId::WifiConfigured, "/api/v1/network/client/configured", 15000),
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
                let Some(n) = self.focused_network(core) else { return true };
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
                let mut input = Overlay::input(Ret::new(ROUTE, "hidden_ssid"), "Hidden network", "Network name (SSID)", "");
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
                core.open(Overlay::confirm(Ret::new(ROUTE, "leave"), "Disconnect", vec![ssid]));
            }
            Command::Forget => {
                let Some(s) = self.focused_saved(core) else { return true };
                if !core.guard_disarmed() {
                    return true;
                }
                core.open(Overlay::confirm(
                    Ret::new(ROUTE, "forget").with(json!(s.name)),
                    "Forget network",
                    vec![format!("Forget {}? Its saved passphrase is deleted.", s.name)],
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
        let ssid_of = |ret: &Ret| ret.data.get("ssid").and_then(Value::as_str).unwrap_or_default().to_string();
        match (ret.tag, result) {
            ("pass", OverlayResult::Text(pass)) => self.confirm_join(core, ssid_of(&ret), Some(pass)),
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
                        Purpose::Page { tag: "leave", data: Value::Null },
                    ));
                }
            }
            ("forget", OverlayResult::Confirmed) => {
                let name = ret.data.as_str().unwrap_or_default().to_string();
                if core.guard_disarmed() {
                    core.submit(Request::new(
                        "DELETE",
                        format!("/api/v1/network/client/configured/{}", crate::api_path::seg(&name)),
                        ROUTE,
                        Purpose::Page { tag: "forget", data: json!(name) },
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
                    Err(e) => core.toast(ToastKind::Error, format!("Scan failed: {}", detail_text(e))),
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
                    Err(e) => core.toast(ToastKind::Error, format!("Disconnect: {}", detail_text(e))),
                }
                core.fetch_now(ResourceId::WifiStatus);
            }
            Purpose::Page { tag: "forget", data } => {
                let name = data.as_str().unwrap_or_default();
                match &reply.result {
                    Ok(_) => core.toast(ToastKind::Success, format!("Forgot {name}")),
                    Err(e) => core.toast(ToastKind::Error, format!("Forget {name}: {}", detail_text(e))),
                }
                core.fetch_now(ResourceId::WifiConfigured);
            }
            Purpose::Page { tag: "autoconnect", data } => {
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
        rows_view::render(f, area, core, &items, &mut self.rows, &kit::prefix(id()), true, out);
    }

    fn context_line(&self, core: &Core) -> Option<String> {
        if let Some(n) = self.focused_network(core) {
            let sec = if n.secured() { n.security.as_str() } else { "open" };
            return Some(format!("{} · signal {} % · {sec} · Enter joins", n.ssid, n.signal));
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
