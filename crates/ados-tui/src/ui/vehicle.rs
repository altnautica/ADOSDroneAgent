//! Tab 2, Vehicle: attitude, heading, GPS, flight state, altitude and battery
//! trends, battery packs, the pilot block (the cockpit's flight-state chip,
//! the pilot's countdowns and banner, mission, link loss, height and the
//! status pill's alerts), the autonomy block, and the sensor/clock row. While
//! a fresh mission stall shows, `s`, `r` and `l` answer it (Skip waypoint,
//! Return, Land here; Land here only for a blocked return) through
//! `POST /api/command`, which hands them to the pilot in ADOS control. While
//! a fresh, disarmed pilot's airframe balance check fails, `b` ("I rebalanced
//! it") asks the pilot to forget the airframe it learned (`airframereset`).

use std::time::Instant;

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::Span;
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

use serde_json::{json, Value};

use crate::api_path::seg;
use crate::app::{Core, FrameOut, Tab, TabId};
use crate::keymap::{Command, Ctx, PageKeys};
use crate::model::autonomy::detection::{
    Detection, Seen, DETECTIONS_LATEST_PATH, VISION_STATUS_PATH,
};
use crate::model::autonomy::{Autonomy, NAV_STATUS_PATH};
use crate::model::mission_stall::{pilot_refusal_text, stall_view, StallAction};
use crate::model::route_check::{other_place_class, place_command, place_label};
use crate::model::skill_detail::{skill_detail_from_state, skill_state_topic};
use crate::model::spec_text::spec_text;
use crate::model::vehicle::{is_msp, Batteries, FcIdent, Telemetry, VehicleHistory};
use crate::poll::{ResourceId, Sub};
use crate::toast::ToastKind;
use crate::worker::{Purpose, Reply, Request, Route};

mod attitude;
mod autonomy;
pub(in crate::ui) mod chart;
mod chip_line;
mod layout;
mod packs;
mod pilot;
mod read;
mod readings;
mod sensor_rows;

use chart::{line_chart, Trace};
use read::Read;

/// What the Vehicle tab shows for an MSP flight controller.
pub const MSP_TEXT: &str =
    "MSP flight controller: live telemetry is decoded in Mission Control and the web dashboard, not by the agent.";

const HIDDEN_STALE: &str = "no fresh data from the agent · flight readings hidden";

/// Where the gimbal says whether it takes rate commands.
const GIMBAL_RATE_PATH: &str = "/api/v1/gimbal/rate";

/// The command route a stall or place action goes out on.
const COMMAND_PATH: &str = "/api/command";
/// The reply tag of a stall action.
const STALL_TAG: &str = "stall";
/// The reply tag of a place-class change.
const PLACE_TAG: &str = "place";
/// The reply tag of "I rebalanced it".
const AIRFRAME_TAG: &str = "airframe";
/// The `/api/command` name of the pilot's `airframe_reset`.
pub(crate) const AIRFRAME_RESET_COMMAND: &str = "airframereset";

/// `POST /api/command` with `cmd`, its words carried for the reply's toast.
fn command_request(tag: &'static str, cmd: &str, label: String) -> Request {
    Request::new(
        "POST",
        COMMAND_PATH,
        Route::tab(TabId::Vehicle),
        Purpose::Page {
            tag,
            data: json!(label),
        },
    )
    .body(json!({ "cmd": cmd }))
}

/// The request a stall action sends: the command that reaches the pilot
/// action.
pub(crate) fn stall_request(action: StallAction) -> Request {
    command_request(STALL_TAG, action.command(), action.label())
}

/// The request that sets the place class (`placeclass_<class>`, the pilot's
/// `place_class:<class>`).
pub(crate) fn place_request(class: &str) -> Request {
    command_request(PLACE_TAG, &place_command(class), place_label(class))
}

/// The request "I rebalanced it" sends: the pilot's `airframe_reset`.
pub(crate) fn airframe_reset_request() -> Request {
    command_request(
        AIRFRAME_TAG,
        AIRFRAME_RESET_COMMAND,
        spec_text("readiness.airframe_balance.reset", &[]),
    )
}

#[derive(Default)]
pub struct VehicleTab {
    tel: Option<Telemetry>,
    fc: FcIdent,
    bat: Option<Batteries>,
    auto: Option<Autonomy>,
    /// The detector's per-camera state; `None` while the engine is
    /// unreachable or does not report it.
    detection: Option<Detection>,
    hist: VehicleHistory,
    nav_read: Read,
    bat_read: Read,
    /// When the pilot's mission turned complete.
    complete_at: Option<Instant>,
    /// The owning skill's state topic, from its plugin's manifest.
    skill_topic: Option<String>,
    /// The owning skill's chip detail (`Orbit 8 m`), from its echoed parameters.
    mode_detail: Option<String>,
    /// When vehicle telemetry was last live, and the arm state then.
    live_at: Option<Instant>,
    armed_at_live: Option<bool>,
    /// The detector's newest fresh batch.
    seen: Option<Seen>,
    /// Whether the gimbal takes rate commands; `None` while unknown.
    gimbal: Option<bool>,
}

impl VehicleTab {
    /// Why live flight readings are not shown, if they are not.
    fn hidden(&self, core: &Core) -> Option<String> {
        if core.stale() {
            return Some(HIDDEN_STALE.into());
        }
        if let Some(err) = core
            .slot(&ResourceId::Telemetry)
            .and_then(|s| s.error.clone())
        {
            return Some(format!("telemetry unavailable: {err}"));
        }
        match &self.tel {
            None => Some("Waiting for telemetry…".into()),
            Some(t) if !t.vehicle => {
                Some("No live vehicle telemetry: the flight controller is not connected".into())
            }
            Some(_) => None,
        }
    }

    /// Whether the detector runs on any camera.
    fn detector_running(&self) -> bool {
        self.detection
            .as_ref()
            .is_some_and(|d| !d.detecting.is_empty())
    }

    fn charts(&self, f: &mut Frame, area: Rect, core: &Core) {
        let t = &core.theme;
        let cols = Layout::horizontal([Constraint::Ratio(1, 4); 4]).split(area);
        let title = |name: &str, v: Option<f64>, unit: &str| match v {
            Some(v) => format!("{name} {v:.1} {unit}"),
            None => name.to_string(),
        };
        let h = &self.hist;
        let specs = [
            (title("Altitude", h.alt.last(), "m"), &h.alt, t.accent()),
            (title("Voltage", h.volts.last(), "V"), &h.volts, t.success()),
            (title("Current", h.amps.last(), "A"), &h.amps, t.warning()),
            (title("Remaining", h.pct.last(), "%"), &h.pct, t.info()),
        ];
        for ((name, ring, color), rect) in specs.into_iter().zip(cols.iter()) {
            line_chart(
                f,
                *rect,
                core,
                &name,
                "-2m",
                &[Trace {
                    name: "",
                    ring,
                    color,
                }],
            );
        }
    }
}

impl Tab for VehicleTab {
    fn ctx(&self, core: &Core) -> Vec<Ctx> {
        let mut ctx = match self.fresh_stall(core).map(|s| s.kind.as_str()) {
            Some("return") => vec![Ctx::Page(PageKeys::ReturnStall)],
            Some(_) => vec![Ctx::Page(PageKeys::Stall)],
            None => Vec::new(),
        };
        if self.place_offered(core).is_some() {
            ctx.push(Ctx::Page(PageKeys::Place));
        }
        if self.airframe_reset_offered(core) {
            ctx.push(Ctx::Page(PageKeys::Airframe));
        }
        ctx
    }

    fn subs(&self, _core: &Core) -> Vec<Sub> {
        let mut subs = vec![
            Sub::every_ms(ResourceId::Telemetry, "/api/telemetry", 1000),
            Sub::every_ms(ResourceId::Snapshot, "/api/v1/dashboard/snapshot", 2000),
            Sub::every_ms(ResourceId::Battery, "/api/v1/battery", 2000),
            Sub::every_ms(ResourceId::NavStatus, NAV_STATUS_PATH, 500),
            Sub::every_ms(ResourceId::VisionStatus, VISION_STATUS_PATH, 1000),
            Sub::every_ms(ResourceId::GimbalRate, GIMBAL_RATE_PATH, 10_000),
        ];
        // The newest batch only while a detector runs: with none running the
        // engine publishes nothing, and each read waits out its timeout on
        // the one thread every other subscription shares.
        if self.detector_running() {
            subs.push(Sub::every_ms(
                ResourceId::DetectionsLatest,
                DETECTIONS_LATEST_PATH,
                1000,
            ));
        }
        // While a skill owns the aircraft: its manifest once (the state
        // topic), then the parameters it echoes, for the chip's detail.
        if let Some((plugin, _)) = self.owner_skill() {
            subs.push(Sub::once(
                ResourceId::PluginDetail(plugin.clone()),
                format!("/api/plugins/{}", seg(plugin)),
            ));
            subs.push(Sub::every_ms(
                ResourceId::PluginState(plugin.clone()),
                format!("/api/plugins/{}/state", seg(plugin)),
                1000,
            ));
        }
        subs
    }

    fn on_resource(&mut self, core: &mut Core, id: &ResourceId) {
        let slot = core.slot(id);
        // An unreachable engine (a 503) is no detector, not the last answer.
        let good = slot
            .filter(|s| s.error.is_none())
            .and_then(|s| s.value.as_ref());
        match id {
            ResourceId::VisionStatus => {
                self.detection = good.and_then(Detection::from_status);
                // A stopped detector's last count is not a current one.
                if !self.detector_running() {
                    self.seen = None;
                }
                if let Some(d) = &mut self.detection {
                    d.seen = self.seen.clone();
                }
                return;
            }
            ResourceId::DetectionsLatest => {
                self.seen = good.and_then(Seen::from_latest);
                if let Some(d) = &mut self.detection {
                    d.seen = self.seen.clone();
                }
                return;
            }
            ResourceId::GimbalRate => {
                // A 404 is an agent without the route: no rate commands. Any
                // other failure (no answer, a 5xx, a refused key) says nothing
                // about the gimbal, so it reads as unknown.
                self.gimbal = match slot {
                    Some(s) if s.error.is_none() => s.value.as_ref().map(|v| {
                        v.get("available").and_then(serde_json::Value::as_bool) == Some(true)
                    }),
                    Some(s) if s.code == Some(404) => Some(false),
                    _ => None,
                };
                return;
            }
            _ => {}
        }
        if let Some(err) = slot.and_then(|s| s.error.clone()) {
            // A failed read keeps the last good model, marked stale.
            match id {
                ResourceId::NavStatus => {
                    if let Some(a) = &mut self.auto {
                        a.mark_stale();
                    }
                    self.nav_read.failed = Some(err);
                }
                ResourceId::Battery => {
                    if let Some(b) = &mut self.bat {
                        b.stale = true;
                    }
                    self.bat_read.failed = Some(err);
                }
                _ => {}
            }
            return;
        }
        let Some(v) = slot.and_then(|s| s.value.as_ref()) else {
            return;
        };
        match id {
            ResourceId::PluginDetail(plugin) => {
                if let Some((_, skill)) = self.owner_skill().filter(|(p, _)| p == plugin) {
                    self.skill_topic = skill_state_topic(v, skill);
                }
            }
            ResourceId::PluginState(plugin) => {
                if self.owner_skill().is_some_and(|(p, _)| p == plugin) {
                    self.mode_detail = self
                        .skill_topic
                        .as_deref()
                        .and_then(|topic| skill_detail_from_state(v, topic));
                }
            }
            ResourceId::Telemetry => {
                let tel = Telemetry::from_value(v);
                self.hist.record(&tel);
                if tel.vehicle {
                    self.live_at = Some(core.now);
                    self.armed_at_live = tel.armed;
                }
                self.tel = Some(tel);
            }
            ResourceId::Snapshot => self.fc = FcIdent::from_snapshot(v),
            ResourceId::Battery => {
                self.bat = Some(Batteries::from_value(v));
                self.bat_read = Read::ok(core.now);
            }
            ResourceId::NavStatus => {
                let a = Autonomy::from_value(v);
                let complete = a
                    .pilot
                    .as_ref()
                    .and_then(|p| p.mission.as_ref())
                    .is_some_and(|m| m.state.as_deref() == Some("complete"));
                // When the mission turned complete, for the chip's
                // MISSION COMPLETE window.
                self.complete_at = match (complete, self.complete_at) {
                    (true, Some(at)) => Some(at),
                    (true, None) => Some(core.now),
                    (false, _) => None,
                };
                let owner = |a: &Autonomy| {
                    a.pilot
                        .as_ref()
                        .filter(|p| p.owner.as_deref() == Some("skill"))
                        .and_then(|p| p.skill_ref.clone())
                };
                // Another owner (or none): the last skill's detail is not this one's.
                if owner(&a) != self.auto.as_ref().and_then(owner) {
                    self.skill_topic = None;
                    self.mode_detail = None;
                }
                self.auto = Some(a);
                self.nav_read = Read::ok(core.now);
            }
            _ => {}
        }
    }

    fn on_command(&mut self, core: &mut Core, cmd: Command) -> bool {
        if cmd == Command::PlaceClass {
            // Only a fresh pilot that reports itself disarmed is asked.
            let Some(current) = self.place_offered(core) else {
                return false;
            };
            core.submit(place_request(other_place_class(current)));
            return true;
        }
        if cmd == Command::AirframeReset {
            if !self.airframe_reset_offered(core) {
                return false;
            }
            core.submit(airframe_reset_request());
            return true;
        }
        let action = match cmd {
            Command::StallSkip => StallAction::Skip,
            Command::StallReturn => StallAction::Return,
            Command::StallLand => StallAction::Land,
            _ => return false,
        };
        // Only a fresh stall that offers this action is answered.
        let offered = self
            .fresh_stall(core)
            .is_some_and(|s| stall_view(s).actions.contains(&action));
        if offered {
            core.submit(stall_request(action));
        }
        offered
    }

    fn on_reply(&mut self, core: &mut Core, reply: Reply) {
        let Purpose::Page {
            tag: STALL_TAG | PLACE_TAG | AIRFRAME_TAG,
            data,
        } = &reply.purpose
        else {
            return;
        };
        let label = data.as_str().unwrap_or_default();
        match &reply.result {
            // The route answers 200 with `status: error` when the pilot
            // refuses, and the reason in `message`.
            Ok(v) if v.get("status").and_then(Value::as_str) == Some("ok") => core.toast(
                ToastKind::Success,
                format!("{label}: {}", spec_text("mav_result.ACCEPTED", &[])),
            ),
            Ok(v) => {
                let why = v
                    .get("message")
                    .and_then(Value::as_str)
                    .map_or_else(|| spec_text("refusal.none", &[]), pilot_refusal_text);
                core.toast(ToastKind::Error, format!("{label}: {why}"));
            }
            Err(e) => core.toast_error(label, e),
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, core: &Core, _out: &mut FrameOut) {
        let t = &core.theme;
        let Some(d) = core.dash() else {
            f.render_widget(
                Paragraph::new(Span::styled("Connecting to the agent…", t.dim())),
                area,
            );
            return;
        };
        if is_msp(d.fc_firmware.as_deref(), d.fc_variant.as_deref()) {
            f.render_widget(
                Paragraph::new(Span::styled(MSP_TEXT, t.text())).wrap(Wrap { trim: false }),
                area,
            );
            return;
        }
        let stale = core.stale();
        let hidden = self.hidden(core);
        let hidden = hidden.as_deref();
        let pilot_rows = self.pilot_lines(core);
        let packs_hidden = stale.then_some(HIDDEN_STALE);
        // The Pilot block takes its rows first; the trend charts go before
        // anything else, and show only while the flight readings are live.
        let (pilot, pilot_min, auto_need) = if packs_hidden.is_some() {
            (3, 3, 3)
        } else {
            (
                pilot_rows.len() as u16 + 2,
                pilot::min_rows(&pilot_rows),
                autonomy::line_count(core, self.auto.as_ref(), self.detection.as_ref()) as u16 + 2,
            )
        };
        let short = layout::is_short(area.height, pilot_min);
        let flight = self.flight_lines(core, hidden, short);
        let gps = self.gps_lines(core, hidden);
        let plan = layout::plan(
            area,
            &layout::Needs {
                pilot,
                pilot_min,
                autonomy: auto_need,
                flight: flight.len() as u16 + 2,
                gps: gps.len() as u16 + 2,
                charts: hidden.is_none(),
            },
        );
        if let Some(r) = plan.attitude {
            attitude::render(f, r, core, self.tel.as_ref(), hidden);
        }
        readings::draw(f, plan.flight, core, "Flight", flight);
        readings::draw(f, plan.gps, core, "GPS", gps);
        if let Some(r) = plan.sensors {
            let lines = self.sensor_lines(core, stale, r.width.saturating_sub(2));
            readings::draw(f, r, core, "Sensors & clock", lines);
        }
        if let Some(r) = plan.charts {
            self.charts(f, r, core);
        }
        packs::render(
            f,
            plan.packs,
            core,
            self.bat.as_ref(),
            packs_hidden,
            self.bat_read.failed_note(core.now).as_deref(),
        );
        pilot::render(f, plan.pilot, core, pilot_rows, packs_hidden);
        autonomy::render(
            f,
            plan.autonomy,
            core,
            self.auto.as_ref(),
            self.detection.as_ref(),
            packs_hidden,
            self.nav_read.failed_note(core.now).as_deref(),
        );
    }

    fn context_line(&self, _core: &Core) -> Option<String> {
        Some("Trends: last 120 s at 1 Hz · attitude and GPS from the flight controller via the agent".into())
    }
}

#[cfg(test)]
mod tests;
