//! Tab 8, System: host resources, agent services, peripherals, the hardware
//! check, and the agent's own lifecycle (update, supervisor restart, reboot).

use std::collections::VecDeque;
use std::time::Instant;

use ratatui::layout::Rect;
use ratatui::Frame;
use serde_json::{json, Value};

use crate::api_path::seg;
use crate::app::{at, Core, FrameOut, Hit, ShellReq, Slot, Tab, TabId};
use crate::keymap::{Command, Ctx, PageKeys};
use crate::overlay::{Overlay, OverlayResult, Ret};
use crate::poll::{ResourceId, Sub};
use crate::toast::ToastKind;
use crate::worker::{Purpose, Request, Route, SCAN_TIMEOUT};

mod view;

/// CPU samples kept for the sparkline (5 min at the 5 s poll).
const CPU_SAMPLES: usize = 60;

const ROUTE: Route = Route::tab(TabId::System);
const TAG_RESTART: &str = "sys.restart";
const TAG_SUPERVISOR: &str = "sys.supervisor";
const TAG_REBOOT: &str = "sys.reboot";
const TAG_RECHECK: &str = "sys.recheck";

/// The focusable lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Pane {
    #[default]
    Services,
    Peripherals,
    Hardware,
}

impl Pane {
    const ALL: [Pane; 3] = [Pane::Services, Pane::Peripherals, Pane::Hardware];
    fn idx(self) -> usize {
        self as usize
    }
}

#[derive(Default)]
pub struct SystemTab {
    pub pane: Pane,
    /// Selected row per pane.
    pub sel: [usize; 3],
    offsets: [usize; 3],
    /// Recent CPU utilisation, oldest first.
    pub cpu: VecDeque<f64>,
    /// A hardware re-check is in flight.
    pub rechecking: bool,
}

/// The array under `key` of a resource body.
pub fn list<'a>(core: &'a Core, id: &ResourceId, key: &str) -> Vec<&'a Value> {
    core.value(id)
        .and_then(|v| v.get(key))
        .and_then(Value::as_array)
        .map(|a| a.iter().collect())
        .unwrap_or_default()
}

pub fn services(core: &Core) -> Vec<&Value> {
    list(core, &ResourceId::Services, "services")
}

pub fn peripherals(core: &Core) -> Vec<&Value> {
    list(core, &ResourceId::Peripherals, "peripherals")
}

pub fn hw_items(core: &Core) -> Vec<&Value> {
    list(core, &ResourceId::HardwareCheck, "items")
}

/// A string field, empty when absent.
pub fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    at(v, key).and_then(Value::as_str).unwrap_or("")
}

/// A peripheral's display name.
pub fn peripheral_name(p: &Value) -> String {
    ["display_name", "name", "manifest.name", "id"]
        .iter()
        .map(|k| s(p, k))
        .find(|n| !n.is_empty())
        .unwrap_or("unnamed")
        .to_string()
}

impl SystemTab {
    fn len(&self, core: &Core, pane: Pane) -> usize {
        match pane {
            Pane::Services => services(core).len(),
            Pane::Peripherals => peripherals(core).len(),
            Pane::Hardware => hw_items(core).len(),
        }
    }

    fn move_sel(&mut self, core: &Core, delta: isize) {
        let len = self.len(core, self.pane);
        let i = self.pane.idx();
        self.sel[i] = if len == 0 {
            0
        } else {
            self.sel[i].saturating_add_signed(delta).min(len - 1)
        };
    }

    fn cycle_pane(&mut self, forward: bool) {
        let i = self.pane.idx();
        let n = Pane::ALL.len();
        self.pane = Pane::ALL[if forward { (i + 1) % n } else { (i + n - 1) % n }];
    }

    /// The selected service's name.
    fn service_name(&self, core: &Core) -> Option<String> {
        services(core)
            .get(self.sel[Pane::Services.idx()])
            .map(|v| s(v, "name").to_string())
            .filter(|n| !n.is_empty())
    }

    fn selected_text(&self, core: &Core) -> Option<String> {
        let i = self.sel[self.pane.idx()];
        match self.pane {
            Pane::Services => self.service_name(core),
            Pane::Peripherals => peripherals(core).get(i).map(|p| peripheral_name(p)),
            Pane::Hardware => hw_items(core).get(i).map(|h| {
                let mut text = format!("{}: {}", s(h, "label"), s(h, "state"));
                for k in ["detail", "fix_hint"] {
                    if !s(h, k).is_empty() {
                        text.push_str(&format!(" · {}", s(h, k)));
                    }
                }
                text
            }),
        }
    }

    fn restart_service(&self, core: &mut Core) {
        let Some(name) = self.service_name(core) else {
            core.toast(ToastKind::Info, "No service selected");
            return;
        };
        if !core.guard_disarmed() {
            return;
        }
        core.open(Overlay::confirm(
            Ret::new(ROUTE, TAG_RESTART).with(json!(name)),
            format!("Restart {name}?"),
            vec!["The unit restarts; whatever it serves drops for a few seconds.".into()],
        ));
    }

    fn reboot(&self, core: &mut Core) {
        if !core.guard_disarmed() {
            return;
        }
        if core.target.remote {
            core.open(Overlay::confirm(
                Ret::new(ROUTE, TAG_REBOOT),
                "Reboot the agent host?",
                vec![
                    format!("{} reboots; this session reconnects when it is back.", core.target.host),
                ],
            ));
        } else {
            // The shell action asks its own y/N on the plain terminal.
            core.shell = Some(ShellReq::Action(crate::action::reboot_index()));
        }
    }

    fn update(&self, core: &mut Core) {
        if core.target.remote {
            core.toast(ToastKind::Warn, "Update runs on the agent host; not available remotely");
            return;
        }
        if core.guard_disarmed() {
            core.shell = Some(ShellReq::Update);
        }
    }

    fn recheck(&mut self, core: &mut Core) {
        if self.rechecking {
            return;
        }
        self.rechecking = true;
        core.submit(
            Request::new(
                "POST",
                "/api/v1/setup/hardware-check/refresh",
                ROUTE,
                Purpose::Page { tag: TAG_RECHECK, data: Value::Null },
            )
            .timeout(SCAN_TIMEOUT),
        );
    }
}

impl Tab for SystemTab {
    fn ctx(&self, _core: &Core) -> Vec<Ctx> {
        vec![Ctx::Page(PageKeys::System), Ctx::List]
    }

    fn subs(&self, _core: &Core) -> Vec<Sub> {
        vec![
            Sub::every_ms(ResourceId::System, "/api/system", 5000),
            Sub::every_ms(ResourceId::Services, "/api/services", 5000),
            Sub::every_ms(ResourceId::Peripherals, "/api/v1/peripherals", 10_000),
            Sub::every_ms(ResourceId::HardwareCheck, "/api/v1/setup/hardware-check", 10_000),
        ]
    }

    fn on_resource(&mut self, core: &mut Core, id: &ResourceId) {
        if *id == ResourceId::System {
            if let Some(cpu) = core.value(id).and_then(|v| v.get("cpu_percent")).and_then(Value::as_f64) {
                self.cpu.push_back(cpu);
                while self.cpu.len() > CPU_SAMPLES {
                    self.cpu.pop_front();
                }
            }
        }
        for pane in Pane::ALL {
            let len = self.len(core, pane);
            let i = pane.idx();
            self.sel[i] = self.sel[i].min(len.saturating_sub(1));
        }
    }

    fn on_command(&mut self, core: &mut Core, cmd: Command) -> bool {
        match cmd {
            Command::Up => self.move_sel(core, -1),
            Command::Down => self.move_sel(core, 1),
            Command::PageUp => self.move_sel(core, -10),
            Command::PageDown => self.move_sel(core, 10),
            Command::Top => self.sel[self.pane.idx()] = 0,
            Command::Bottom => self.move_sel(core, isize::MAX),
            Command::NextPane | Command::Right => self.cycle_pane(true),
            Command::PrevPane | Command::Left => self.cycle_pane(false),
            Command::Activate => match self.selected_text(core) {
                Some(text) => core.open(Overlay::result("Detail", vec![(ToastKind::Info, text)])),
                None => return true,
            },
            Command::Copy => match self.selected_text(core) {
                Some(text) => core.copy(&text),
                None => core.toast(ToastKind::Info, "Nothing selected"),
            },
            Command::RestartService => self.restart_service(core),
            Command::Recheck => self.recheck(core),
            Command::RestartSupervisor => {
                if core.guard_disarmed() {
                    core.open(Overlay::confirm(
                        Ret::new(ROUTE, TAG_SUPERVISOR),
                        "Restart the agent supervisor?",
                        vec!["Every agent service restarts; video, radio and MAVLink drop briefly.".into()],
                    ));
                }
            }
            Command::Reboot => self.reboot(core),
            Command::Update => self.update(core),
            _ => return false,
        }
        core.dirty = true;
        true
    }

    fn on_overlay(&mut self, core: &mut Core, ret: Ret, result: OverlayResult) {
        if result != OverlayResult::Confirmed || !core.guard_disarmed() {
            return;
        }
        match ret.tag {
            TAG_RESTART => {
                let Some(name) = ret.data.as_str().map(str::to_string) else { return };
                core.submit(Request::new(
                    "POST",
                    format!("/api/services/{}/restart", seg(&name)),
                    ROUTE,
                    Purpose::Page { tag: TAG_RESTART, data: json!(name) },
                ));
            }
            TAG_SUPERVISOR => core.submit(Request::new(
                "POST",
                "/api/v1/system/restart-supervisor",
                ROUTE,
                Purpose::Page { tag: TAG_SUPERVISOR, data: Value::Null },
            )),
            TAG_REBOOT => core.submit(Request::new(
                "POST",
                "/api/v1/setup/reboot",
                ROUTE,
                Purpose::Generic { ok_toast: "Reboot requested".into() },
            )),
            _ => {}
        }
    }

    fn on_reply(&mut self, core: &mut Core, reply: crate::worker::Reply) {
        let Purpose::Page { tag, data } = &reply.purpose else { return };
        match (*tag, &reply.result) {
            (TAG_RESTART, Ok(body)) => {
                let name = data.as_str().unwrap_or("service");
                if body.get("status").and_then(Value::as_str) == Some("ok") {
                    core.toast(ToastKind::Success, format!("{name} restarted"));
                } else {
                    let msg = body.get("message").and_then(Value::as_str).unwrap_or("restart failed");
                    core.toast(ToastKind::Error, format!("{name}: {msg}"));
                }
                core.fetch_now(ResourceId::Services);
            }
            (TAG_SUPERVISOR, Ok(body)) => {
                let msg = body.get("message").and_then(Value::as_str);
                if body.get("ok").and_then(Value::as_bool) == Some(true) {
                    core.toast(ToastKind::Success, msg.unwrap_or("Supervisor restart scheduled").to_string());
                } else {
                    core.toast(ToastKind::Error, msg.unwrap_or("Supervisor restart failed").to_string());
                }
            }
            (TAG_RECHECK, Ok(body)) => {
                self.rechecking = false;
                core.res.insert(
                    ResourceId::HardwareCheck,
                    Slot { value: Some(body.clone()), error: None, code: None, error_body: None, at: Instant::now() },
                );
                core.toast(ToastKind::Success, "Hardware check refreshed");
            }
            (TAG_RESTART, Err(e)) => core.toast_error("Restart", e),
            (TAG_SUPERVISOR, Err(e)) => core.toast_error("Supervisor restart", e),
            (TAG_RECHECK, Err(e)) => {
                self.rechecking = false;
                core.toast_error("Hardware check", e);
            }
            _ => {}
        }
        core.dirty = true;
    }

    fn on_click(&mut self, core: &mut Core, hit: &Hit) {
        if let Hit::Item(name, i) = hit {
            let pane = match *name {
                "svc" => Pane::Services,
                "per" => Pane::Peripherals,
                "hw" => Pane::Hardware,
                _ => return,
            };
            self.pane = pane;
            self.sel[pane.idx()] = *i;
            core.dirty = true;
        }
    }

    fn animating(&self, _core: &Core) -> bool {
        self.rechecking
    }

    fn render(&mut self, f: &mut Frame, area: Rect, core: &Core, out: &mut FrameOut) {
        self.offsets = view::render(f, area, core, self, out);
    }

    fn context_line(&self, core: &Core) -> Option<String> {
        let i = self.sel[self.pane.idx()];
        match self.pane {
            Pane::Services => services(core).get(i).map(|v| {
                format!(
                    "{}: {} / {} · R restarts it",
                    s(v, "name"),
                    s(v, "state"),
                    s(v, "sub_state")
                )
            }),
            Pane::Peripherals => peripherals(core).get(i).and_then(|p| {
                let d = s(p, "manifest.description");
                (!d.is_empty()).then(|| d.to_string())
            }),
            Pane::Hardware => hw_items(core).get(i).and_then(|h| {
                let hint = s(h, "fix_hint");
                let text = if hint.is_empty() { s(h, "detail") } else { hint };
                (!text.is_empty()).then(|| text.to_string())
            }),
        }
    }
}

#[cfg(test)]
mod tests;
