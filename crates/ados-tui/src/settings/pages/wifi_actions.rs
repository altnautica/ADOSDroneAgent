//! The Wi-Fi page's own logic: the row list, scan, join, and saved-profile
//! autoconnect. Rendering and key routing stay in `wifi.rs`.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use serde_json::{json, Value};

use super::kit::{self, detail_text, text_of};
use super::wifi::{Page, Pending, ROUTE};
use super::wifi_model::{self as model, JoinConflict, Network, Saved};
use crate::app::{Core, ARMED_REFUSAL};
use crate::overlay::{Overlay, Ret};
use crate::poll::ResourceId;
use crate::settings::rows_view::{info, note, Item};
use crate::settings::schema::Kind;
use crate::toast::ToastKind;
use crate::worker::{self, Purpose, Request};

impl Page {
    pub(super) fn saved(core: &Core) -> Vec<Saved> {
        core.value(&ResourceId::WifiConfigured)
            .map(model::parse_saved)
            .unwrap_or_default()
    }

    pub(super) fn items(&self, core: &Core) -> Vec<Item> {
        let t = &core.theme;
        let mut items = vec![Item::Header("Status".into())];
        let slot = core.slot(&ResourceId::WifiStatus);
        match slot {
            Some(s)
                if s.value.is_none()
                    && model::status_unavailable(s.code, s.error_body.as_ref()) =>
            {
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
                    .map(|s| {
                        format!(
                            "{} {s:.0} %",
                            core.sym.signal_bars(s.clamp(0.0, 100.0) as u8)
                        )
                    })
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
                let w = nets
                    .iter()
                    .map(|n| n.ssid.chars().count())
                    .max()
                    .unwrap_or(0)
                    .min(32);
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
    pub(super) fn toggle_autoconnect(&self, core: &mut Core) -> bool {
        let Some(s) = self.focused_saved(core) else {
            return false;
        };
        let enabled = !s.autoconnect;
        core.submit(
            Request::new(
                "PUT",
                format!(
                    "/api/v1/network/client/configured/{}/autoconnect",
                    crate::api_path::seg(&s.name)
                ),
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

    pub(super) fn scan(&mut self, core: &mut Core) {
        if self.scanning {
            return;
        }
        self.scanning = true;
        core.submit(
            Request::new(
                "GET",
                "/api/v1/network/client/scan",
                ROUTE,
                Purpose::WifiScan,
            )
            .timeout(worker::SCAN_TIMEOUT),
        );
    }

    pub(super) fn confirm_join(
        &mut self,
        core: &mut Core,
        ssid: String,
        passphrase: Option<String>,
    ) {
        let mut body = vec![format!("Join {ssid}?")];
        if core.target.remote {
            body.push("This session may disconnect if the agent changes networks.".into());
        }
        self.pending = Some(Pending { ssid, passphrase });
        core.open(Overlay::confirm(
            Ret::new(ROUTE, "join"),
            "Join network",
            body,
        ));
    }

    pub(super) fn send_join(&self, core: &mut Core, forced: bool) {
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

    pub(super) fn join_reply(
        &mut self,
        core: &mut Core,
        ssid: &str,
        forced: bool,
        result: &Result<Value, ados_protocol::rest::RestError>,
    ) {
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

    pub(super) fn focused_network(&self, core: &Core) -> Option<Network> {
        let items = self.items(core);
        let key = self
            .rows
            .focused_key(&items)?
            .strip_prefix("net:")?
            .to_string();
        self.networks
            .as_ref()?
            .iter()
            .find(|n| n.ssid == key)
            .cloned()
    }

    pub(super) fn focused_saved(&self, core: &Core) -> Option<Saved> {
        let items = self.items(core);
        let name = self
            .rows
            .focused_key(&items)?
            .strip_prefix("saved:")?
            .to_string();
        Self::saved(core).into_iter().find(|s| s.name == name)
    }
}
