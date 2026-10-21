//! Tab 2 on a ground station, Ground: GNSS receivers and the RTK base,
//! gamepads, Bluetooth, phone receiver invites, and the display.

use std::time::Duration;

use ratatui::layout::Rect;
use ratatui::Frame;
use serde_json::{json, Value};

use crate::api_path::seg;
use crate::app::{Core, FrameOut, Hit, Tab, TabId};
use crate::keymap::{Command, Ctx, PageKeys};
use crate::overlay::{Overlay, OverlayResult, Ret};
use crate::poll::{ResourceId, Sub};
use crate::toast::ToastKind;
use crate::worker::{Purpose, Reply, Request, Route};

mod view;

const ROUTE: Route = Route::tab(TabId::Vehicle);
/// A Bluetooth scan blocks for its duration plus up to four 5 s
/// `bluetoothctl` steps.
const BT_SCAN_TIMEOUT: Duration = Duration::from_secs(45);
/// Pairing runs pair (30 s), trust (5 s) and connect (15 s) in turn.
const BT_PAIR_TIMEOUT: Duration = Duration::from_secs(60);
const SCAN_SECONDS: u64 = 8;

/// The focusable lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Pane {
    #[default]
    Gamepads,
    Bluetooth,
    Invites,
}

impl Pane {
    const ALL: [Pane; 3] = [Pane::Gamepads, Pane::Bluetooth, Pane::Invites];
    fn index(self) -> usize {
        Self::ALL.iter().position(|p| *p == self).unwrap_or(0)
    }
    fn step(self, forward: bool) -> Self {
        let i = self.index();
        let n = Self::ALL.len();
        Self::ALL[if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        }]
    }
}

/// One Bluetooth row: a paired device, or a scan result not yet paired.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BtRow {
    pub mac: String,
    pub name: String,
    pub paired: bool,
    pub connected: Option<bool>,
}

#[derive(Default)]
pub struct GroundTab {
    pane: Pane,
    sel: [usize; 3],
    scan: Vec<(String, String)>,
    scanning: bool,
    pairing: Option<String>,
}

fn arr<'a>(core: &'a Core, id: &ResourceId, key: &str) -> &'a [Value] {
    core.value(id)
        .and_then(|v| v.get(key))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

impl GroundTab {
    pub(crate) fn bt_rows(&self, core: &Core) -> Vec<BtRow> {
        let mut rows: Vec<BtRow> = arr(core, &ResourceId::BtPaired, "devices")
            .iter()
            .map(|d| BtRow {
                mac: text(d, "mac").to_string(),
                name: text(d, "name").to_string(),
                paired: true,
                connected: d.get("connected").and_then(Value::as_bool),
            })
            .collect();
        for (mac, name) in &self.scan {
            if !rows.iter().any(|r| r.mac.eq_ignore_ascii_case(mac)) {
                rows.push(BtRow {
                    mac: mac.clone(),
                    name: name.clone(),
                    paired: false,
                    connected: None,
                });
            }
        }
        rows
    }

    fn len(&self, core: &Core, pane: Pane) -> usize {
        match pane {
            Pane::Gamepads => arr(core, &ResourceId::Gamepads, "devices").len(),
            Pane::Bluetooth => self.bt_rows(core).len(),
            Pane::Invites => arr(core, &ResourceId::Invites, "pending").len(),
        }
    }

    fn selected(&self, core: &Core, pane: Pane) -> Option<usize> {
        let n = self.len(core, pane);
        (n > 0).then(|| self.sel[pane.index()].min(n - 1))
    }

    fn mv(&mut self, core: &Core, delta: isize) {
        let n = self.len(core, self.pane);
        let s = &mut self.sel[self.pane.index()];
        *s = if n == 0 {
            0
        } else {
            (*s as isize + delta).clamp(0, n as isize - 1) as usize
        };
    }

    fn invite(&self, core: &Core) -> Option<Value> {
        let i = self.selected(core, Pane::Invites)?;
        arr(core, &ResourceId::Invites, "pending").get(i).cloned()
    }

    fn activate(&mut self, core: &mut Core) {
        match self.pane {
            Pane::Gamepads => {
                let Some(i) = self.selected(core, Pane::Gamepads) else {
                    return;
                };
                let Some(id) = arr(core, &ResourceId::Gamepads, "devices")
                    .get(i)
                    .map(|d| text(d, "device_id").to_string())
                else {
                    return;
                };
                core.submit(
                    Request::new(
                        "PUT",
                        "/api/v1/ground-station/gamepads/primary",
                        ROUTE,
                        Purpose::Generic {
                            ok_toast: "Primary gamepad set".into(),
                        },
                    )
                    .body(json!({ "device_id": id })),
                );
            }
            Pane::Bluetooth => {
                let Some(i) = self.selected(core, Pane::Bluetooth) else {
                    return;
                };
                let Some(row) = self.bt_rows(core).into_iter().nth(i) else {
                    return;
                };
                if row.paired {
                    core.toast(ToastKind::Info, format!("{} is already paired", row.name));
                    return;
                }
                if self.pairing.is_some() {
                    core.toast(ToastKind::Info, "A pairing is already running");
                    return;
                }
                self.pairing = Some(row.name.clone());
                core.submit(
                    Request::new(
                        "POST",
                        "/api/v1/ground-station/bluetooth/pair",
                        ROUTE,
                        Purpose::Page {
                            tag: "bt_pair",
                            data: json!({ "name": row.name }),
                        },
                    )
                    .body(json!({ "mac": row.mac }))
                    .timeout(BT_PAIR_TIMEOUT),
                );
            }
            Pane::Invites => self.approve(core),
        }
    }

    fn approve(&mut self, core: &mut Core) {
        let Some(inv) = self.invite(core) else {
            core.toast(ToastKind::Info, "No pending invite");
            return;
        };
        let body = vec![
            format!("Label: {}", text(&inv, "label")),
            format!("Phone fingerprint: {}", text(&inv, "phone_fingerprint")),
            "Check the fingerprint matches the phone before approving.".into(),
        ];
        let ret = Ret::new(ROUTE, "approve").with(json!({ "id": text(&inv, "invite_id") }));
        core.open(Overlay::confirm(ret, "Approve the phone receiver?", body));
    }

    fn reject(&mut self, core: &mut Core) {
        let Some(inv) = self.invite(core) else {
            core.toast(ToastKind::Info, "No pending invite");
            return;
        };
        let id = text(&inv, "invite_id").to_string();
        core.submit(Request::new(
            "POST",
            format!("/api/v1/ground-station/wfb/invite/{}/reject", seg(&id)),
            ROUTE,
            Purpose::Generic {
                ok_toast: "Invite rejected".into(),
            },
        ));
    }

    fn survey_in(&mut self, core: &mut Core) {
        let base = core.value(&ResourceId::Gnss).and_then(|g| g.get("base"));
        let dur = base
            .and_then(|b| b.get("min_dur_s"))
            .and_then(Value::as_u64)
            .unwrap_or(300);
        let mut input = Overlay::input(
            Ret::new(ROUTE, "survey_dur"),
            "Start survey-in",
            "Minimum duration (s)",
            &dur.to_string(),
        );
        input.validate = Some(valid_duration);
        core.open(Overlay::Input(input));
    }

    fn base_request(core: &mut Core, body: Value, ok: &str) {
        core.submit(
            Request::new(
                "PUT",
                "/api/v1/gnss/base",
                ROUTE,
                Purpose::Page {
                    tag: "base",
                    data: json!({ "ok": ok }),
                },
            )
            .body(body),
        );
    }
}

fn valid_duration(s: &str) -> Result<(), String> {
    match s.trim().parse::<u32>() {
        Ok(n) if n >= 1 => Ok(()),
        _ => Err("A whole number of seconds, 1 or more".into()),
    }
}

fn valid_accuracy(s: &str) -> Result<(), String> {
    match s.trim().parse::<f64>() {
        Ok(v) if v.is_finite() && v > 0.0 => Ok(()),
        _ => Err("A distance in metres, above 0".into()),
    }
}

impl Tab for GroundTab {
    fn ctx(&self, _core: &Core) -> Vec<Ctx> {
        vec![Ctx::Page(PageKeys::Ground), Ctx::List]
    }

    fn subs(&self, _core: &Core) -> Vec<Sub> {
        vec![
            Sub::every_ms(ResourceId::Gnss, "/api/v1/gnss", 2000),
            Sub::every_ms(
                ResourceId::Gamepads,
                "/api/v1/ground-station/gamepads",
                5000,
            ),
            Sub::every_ms(
                ResourceId::BtPaired,
                "/api/v1/ground-station/bluetooth/paired",
                10_000,
            ),
            Sub::every_ms(
                ResourceId::Invites,
                "/api/v1/ground-station/wfb/invite",
                2000,
            ),
            Sub::once(ResourceId::GsDisplay, "/api/v1/ground-station/display"),
        ]
    }

    fn on_command(&mut self, core: &mut Core, cmd: Command) -> bool {
        match cmd {
            Command::Up => self.mv(core, -1),
            Command::Down => self.mv(core, 1),
            Command::PageUp => self.mv(core, -10),
            Command::PageDown => self.mv(core, 10),
            Command::Top => self.mv(core, isize::MIN / 2),
            Command::Bottom => self.mv(core, isize::MAX / 2),
            Command::NextPane | Command::Right => self.pane = self.pane.step(true),
            Command::PrevPane | Command::Left => self.pane = self.pane.step(false),
            Command::Activate => self.activate(core),
            Command::SurveyIn => self.survey_in(core),
            Command::BaseOff => core.open(Overlay::confirm(
                Ret::new(ROUTE, "base_off"),
                "Turn the RTK base off?",
                vec!["The receiver stops sending corrections.".into()],
            )),
            Command::BtScan => {
                if self.scanning {
                    core.toast(ToastKind::Info, "A Bluetooth scan is already running");
                } else {
                    self.scanning = true;
                    core.submit(
                        Request::new(
                            "POST",
                            "/api/v1/ground-station/bluetooth/scan",
                            ROUTE,
                            Purpose::Page {
                                tag: "bt_scan",
                                data: Value::Null,
                            },
                        )
                        .body(json!({ "duration_s": SCAN_SECONDS }))
                        .timeout(BT_SCAN_TIMEOUT),
                    );
                }
            }
            Command::Approve => self.approve(core),
            Command::Reject => self.reject(core),
            _ => return false,
        }
        core.dirty = true;
        true
    }

    fn on_overlay(&mut self, core: &mut Core, ret: Ret, result: OverlayResult) {
        match (ret.tag, result) {
            ("survey_dur", OverlayResult::Text(s)) => {
                let dur: u32 = s.trim().parse().unwrap_or(300);
                let acc = core
                    .value(&ResourceId::Gnss)
                    .and_then(|g| g.pointer("/base/target_accuracy_m"))
                    .and_then(Value::as_f64)
                    .unwrap_or(2.0);
                let mut input = Overlay::input(
                    Ret::new(ROUTE, "survey_acc").with(json!({ "min_dur_s": dur })),
                    "Start survey-in",
                    "Target accuracy (m)",
                    &acc.to_string(),
                );
                input.validate = Some(valid_accuracy);
                core.open(Overlay::Input(input));
            }
            ("survey_acc", OverlayResult::Text(s)) => {
                let acc: f64 = s.trim().parse().unwrap_or(2.0);
                let dur = ret.data.get("min_dur_s").cloned().unwrap_or(json!(300));
                Self::base_request(
                    core,
                    json!({"mode": "survey", "min_dur_s": dur, "accuracy_m": acc}),
                    "Survey-in started",
                );
            }
            ("base_off", OverlayResult::Confirmed) => {
                Self::base_request(core, json!({"mode": "off"}), "Base off");
            }
            ("approve", OverlayResult::Confirmed) => {
                let id = ret
                    .data
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                core.submit(Request::new(
                    "POST",
                    format!("/api/v1/ground-station/wfb/invite/{}/approve", seg(id)),
                    ROUTE,
                    Purpose::Generic {
                        ok_toast: "Invite approved".into(),
                    },
                ));
            }
            _ => {}
        }
    }

    fn on_reply(&mut self, core: &mut Core, reply: Reply) {
        match &reply.purpose {
            Purpose::Generic { ok_toast } if ok_toast.starts_with("Invite") => {
                core.fetch_now(ResourceId::Invites)
            }
            Purpose::Generic { .. } => core.fetch_now(ResourceId::Gamepads),
            Purpose::Page { tag: "base", data } => {
                match &reply.result {
                    Ok(_) => core.toast(
                        ToastKind::Success,
                        data["ok"].as_str().unwrap_or("Base updated"),
                    ),
                    Err(e) => core.toast_error("RTK base", e),
                }
                core.fetch_now(ResourceId::Gnss);
            }
            Purpose::Page { tag: "bt_scan", .. } => {
                self.scanning = false;
                match &reply.result {
                    Ok(v) => {
                        self.scan = v
                            .get("devices")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .map(|d| (text(d, "mac").to_string(), text(d, "name").to_string()))
                            .filter(|(mac, _)| !mac.is_empty())
                            .collect();
                        let n = self.scan.len();
                        core.toast(
                            ToastKind::Info,
                            format!("Scan found {n} device{}", if n == 1 { "" } else { "s" }),
                        );
                        self.pane = Pane::Bluetooth;
                    }
                    Err(e) => core.toast_error("Bluetooth scan", e),
                }
                core.fetch_now(ResourceId::BtPaired);
            }
            Purpose::Page {
                tag: "bt_pair",
                data,
            } => {
                self.pairing = None;
                let name = data["name"].as_str().unwrap_or("device");
                match &reply.result {
                    Ok(v) => {
                        let err = v.get("error").and_then(Value::as_str).unwrap_or("");
                        match (
                            v.get("paired").and_then(Value::as_bool),
                            v.get("connected").and_then(Value::as_bool),
                        ) {
                            (Some(true), Some(true)) => {
                                core.toast(ToastKind::Success, format!("Paired {name}"))
                            }
                            (Some(true), _) => core.toast(
                                ToastKind::Warn,
                                format!("Paired {name}, not connected: {err}"),
                            ),
                            _ => core
                                .toast(ToastKind::Error, format!("Pairing {name} failed: {err}")),
                        }
                    }
                    Err(e) => core.toast_error("Bluetooth pair", e),
                }
                core.fetch_now(ResourceId::BtPaired);
            }
            _ => {}
        }
    }

    fn on_click(&mut self, core: &mut Core, hit: &Hit) {
        if let Hit::Item(name, i) = hit {
            let pane = match *name {
                "gamepad" => Pane::Gamepads,
                "bt" => Pane::Bluetooth,
                "invite" => Pane::Invites,
                _ => return,
            };
            self.pane = pane;
            self.sel[pane.index()] = *i;
            core.dirty = true;
        }
    }

    fn animating(&self, _core: &Core) -> bool {
        self.scanning || self.pairing.is_some()
    }

    fn render(&mut self, f: &mut Frame, area: Rect, core: &Core, out: &mut FrameOut) {
        view::render(self, f, area, core, out);
    }

    fn context_line(&self, _core: &Core) -> Option<String> {
        Some(
            match self.pane {
                Pane::Gamepads => "Enter makes the selected gamepad the primary flight input",
                Pane::Bluetooth => "s scans for 8 s · Enter pairs a scanned device",
                Pane::Invites => {
                    "a approves (after checking the fingerprint) · x rejects a phone receiver"
                }
            }
            .into(),
        )
    }
}

#[cfg(test)]
mod tests;
