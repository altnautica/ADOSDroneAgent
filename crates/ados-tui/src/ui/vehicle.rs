//! Tab 2, Vehicle: attitude, heading, GPS, flight state, altitude and battery
//! trends, battery packs, and the sensor/clock row.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{Core, FrameOut, Tab};
use crate::keymap::{Command, Ctx};
use crate::model::fix_label;
use crate::model::vehicle::{is_msp, Batteries, FcIdent, Telemetry, VehicleHistory};
use crate::poll::{ResourceId, Sub};
use crate::ui::widgets::{chip, kv, kv_text, panel};

mod attitude;
pub(in crate::ui) mod chart;
mod packs;

use chart::{line_chart, Trace};

/// What the Vehicle tab shows for an MSP flight controller.
pub const MSP_TEXT: &str =
    "MSP flight controller: live telemetry is decoded in Mission Control and the web dashboard, not by the agent.";

const HIDDEN_STALE: &str = "no fresh data from the agent · flight readings hidden";

#[derive(Default)]
pub struct VehicleTab {
    tel: Option<Telemetry>,
    fc: FcIdent,
    bat: Option<Batteries>,
    hist: VehicleHistory,
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

    fn flight_panel(&self, f: &mut Frame, area: Rect, core: &Core, hidden: Option<&str>) {
        let t = &core.theme;
        let block = panel(t, core.sym, "Flight");
        let inner = block.inner(area);
        f.render_widget(block, area);
        let mut lines = Vec::new();
        let ident = [self.fc.vehicle.clone(), self.fc.firmware.clone()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
        if !ident.is_empty() {
            lines.push(kv_text(t, "Vehicle", 9, ident));
        }
        match (hidden, &self.tel) {
            (None, Some(tel)) => {
                let armed = match tel.armed {
                    Some(true) => chip(t, "ARMED", t.error()),
                    Some(false) => chip(t, "DISARMED", t.success()),
                    None => Span::styled("arm state unknown", t.dim()),
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("{:<9} ", "Mode"), t.dim()),
                    Span::styled(
                        format!("{}  ", tel.mode.as_deref().unwrap_or("—")),
                        t.bright(),
                    ),
                    armed,
                ]));
                let heading = tel
                    .heading
                    .map(|h| format!("{h:03.0}° {}", compass(h)))
                    .unwrap_or_else(|| "—".into());
                lines.push(kv_text(t, "Heading", 9, heading));
                let mut speed = Vec::new();
                if let Some(v) = tel.groundspeed {
                    speed.push(format!("GS {v:.1} m/s"));
                }
                if let Some(v) = tel.climb {
                    speed.push(format!("VS {v:+.1} m/s"));
                }
                if !speed.is_empty() {
                    lines.push(kv_text(t, "Speed", 9, speed.join(" · ")));
                }
                let mut bat = Vec::new();
                for (v, unit) in [(tel.voltage, "V"), (tel.current, "A"), (tel.remaining, "%")] {
                    if let Some(v) = v {
                        bat.push(format!("{v:.1} {unit}"));
                    }
                }
                if let Some(c) = tel.temperature {
                    bat.push(format!("{c:.0} °C"));
                }
                if !bat.is_empty() {
                    lines.push(kv_text(t, "Battery", 9, bat.join(" · ")));
                }
                if tel.relayed {
                    lines.push(kv(
                        t,
                        "Source",
                        9,
                        Span::styled("relayed by a peer", Style::default().fg(t.info())),
                    ));
                }
            }
            (why, _) => lines.push(Line::from(Span::styled(
                why.unwrap_or_default().to_string(),
                t.dim(),
            ))),
        }
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }

    fn gps_panel(&self, f: &mut Frame, area: Rect, core: &Core, hidden: Option<&str>) {
        let t = &core.theme;
        let block = panel(t, core.sym, "GPS");
        let inner = block.inner(area);
        f.render_widget(block, area);
        let lines = match (hidden, &self.tel) {
            (None, Some(tel)) => {
                let fix = tel.fix_type.map(fix_label).unwrap_or_else(|| "—".into());
                let fix_color = match tel.fix_type {
                    Some(n) if n >= 3 => t.success(),
                    Some(2) => t.warning(),
                    _ => t.error(),
                };
                let hdop = tel.eph.or(self.fc.hdop);
                let mut l = vec![Line::from(vec![
                    Span::styled(format!("{:<9} ", "Fix"), t.dim()),
                    Span::styled(fix, Style::default().fg(fix_color)),
                    Span::styled("  sats ", t.dim()),
                    Span::styled(
                        tel.satellites
                            .map(|n| n.to_string())
                            .unwrap_or_else(|| "—".into()),
                        t.bright(),
                    ),
                    Span::styled("  HDOP ", t.dim()),
                    Span::styled(
                        hdop.map(|h| format!("{h:.1}"))
                            .unwrap_or_else(|| "—".into()),
                        t.bright(),
                    ),
                ])];
                match tel.lat.zip(tel.lon) {
                    Some((la, lo)) => {
                        l.push(kv_text(t, "Position", 9, format!("{la:.6}, {lo:.6}")))
                    }
                    None => l.push(kv(
                        t,
                        "Position",
                        9,
                        Span::styled("no position yet", t.dim()),
                    )),
                }
                let mut alt = Vec::new();
                if let Some(a) = tel.alt_rel {
                    alt.push(format!("{a:.1} m rel"));
                }
                if let Some(a) = tel.alt_msl {
                    alt.push(format!("{a:.1} m MSL"));
                }
                if !alt.is_empty() {
                    l.push(kv_text(t, "Altitude", 9, alt.join(" · ")));
                }
                l
            }
            (why, _) => vec![Line::from(Span::styled(
                why.unwrap_or_default().to_string(),
                t.dim(),
            ))],
        };
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
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

    fn sensors(&self, f: &mut Frame, area: Rect, core: &Core, stale: bool) {
        let t = &core.theme;
        let block = panel(t, core.sym, "Sensors & clock");
        let inner = block.inner(area);
        f.render_widget(block, area);
        if stale {
            f.render_widget(
                Paragraph::new(Span::styled("readings hidden (stale)", t.dim())),
                inner,
            );
            return;
        }
        let Some(d) = core.dash() else { return };
        let mut lines = Vec::new();
        let conn = |v: Option<bool>| match v {
            Some(true) => Span::styled(
                format!("{} connected", core.sym.ok),
                Style::default().fg(t.success()),
            ),
            Some(false) => Span::styled(
                format!("{} not connected", core.sym.err),
                Style::default().fg(t.error()),
            ),
            None => Span::styled("not reported", t.dim()),
        };
        lines.push(kv(t, "Hub", 7, conn(d.hub_connected)));
        lines.push(kv(t, "FC", 7, conn(d.sensors_fc_connected)));
        lines.extend(crate::ui::profile_row::packed_lines(t, d, inner.width));
        f.render_widget(Paragraph::new(lines), inner);
    }
}

/// An 8-point compass word for a heading in degrees.
fn compass(deg: f64) -> &'static str {
    const POINTS: [&str; 8] = ["N", "NE", "E", "SE", "S", "SW", "W", "NW"];
    POINTS[((deg.rem_euclid(360.0) + 22.5) / 45.0) as usize % 8]
}

impl Tab for VehicleTab {
    fn ctx(&self, _core: &Core) -> Vec<Ctx> {
        Vec::new()
    }

    fn subs(&self, _core: &Core) -> Vec<Sub> {
        vec![
            Sub::every_ms(ResourceId::Telemetry, "/api/telemetry", 1000),
            Sub::every_ms(ResourceId::Snapshot, "/api/v1/dashboard/snapshot", 2000),
            Sub::every_ms(ResourceId::Battery, "/api/v1/battery", 2000),
        ]
    }

    fn on_resource(&mut self, core: &mut Core, id: &ResourceId) {
        let Some(v) = core
            .slot(id)
            .filter(|s| s.error.is_none())
            .and_then(|s| s.value.as_ref())
        else {
            return;
        };
        match id {
            ResourceId::Telemetry => {
                let tel = Telemetry::from_value(v);
                self.hist.record(&tel);
                self.tel = Some(tel);
            }
            ResourceId::Snapshot => self.fc = FcIdent::from_snapshot(v),
            ResourceId::Battery => self.bat = Some(Batteries::from_value(v)),
            _ => {}
        }
    }

    fn on_command(&mut self, _core: &mut Core, _cmd: Command) -> bool {
        false
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
        let rows = Layout::vertical([
            Constraint::Length(12),
            Constraint::Length(9),
            Constraint::Min(4),
        ])
        .split(area);
        let top = Layout::horizontal([
            Constraint::Length(36),
            Constraint::Min(30),
            Constraint::Length(40),
        ])
        .split(rows[0]);
        attitude::render(f, top[0], core, self.tel.as_ref(), hidden);
        let mid = Layout::vertical([Constraint::Length(7), Constraint::Min(3)]).split(top[1]);
        self.flight_panel(f, mid[0], core, hidden);
        self.gps_panel(f, mid[1], core, hidden);
        self.sensors(f, top[2], core, stale);
        self.charts(f, rows[1], core);
        let packs_hidden = stale.then_some(HIDDEN_STALE);
        packs::render(f, rows[2], core, self.bat.as_ref(), packs_hidden);
    }

    fn context_line(&self, _core: &Core) -> Option<String> {
        Some("Trends: last 120 s at 1 Hz · attitude and GPS from the flight controller via the agent".into())
    }
}

#[cfg(test)]
mod tests;
