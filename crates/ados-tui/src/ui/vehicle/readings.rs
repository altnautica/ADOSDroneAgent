//! The Flight, GPS and Sensors panels' lines. Each line is cut with a `…` mark
//! to its panel's width, so a narrow terminal never wraps a row into the next
//! one or clips a value unmarked.

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::Core;
use crate::ui::widgets::{chip, kv, kv_text, panel, truncate_line};

use super::autonomy::fix_word;
use super::chip_line::compass;
use super::VehicleTab;

/// Draw `lines` in a titled panel, each cut to the panel's width.
pub(super) fn draw(f: &mut Frame, area: Rect, core: &Core, title: &str, lines: Vec<Line<'static>>) {
    let block = panel(&core.theme, core.sym, title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let width = usize::from(inner.width);
    let lines: Vec<Line> = lines.into_iter().map(|l| truncate_line(l, width)).collect();
    f.render_widget(Paragraph::new(lines), inner);
}

fn why_line(core: &Core, why: Option<&str>) -> Vec<Line<'static>> {
    vec![Line::from(Span::styled(
        why.unwrap_or_default().to_string(),
        core.theme.dim(),
    ))]
}

impl VehicleTab {
    /// The Flight panel. `attitude` adds the roll and pitch row, for a layout
    /// that has no room for the attitude indicator.
    pub(super) fn flight_lines(
        &self,
        core: &Core,
        hidden: Option<&str>,
        attitude: bool,
    ) -> Vec<Line<'static>> {
        let t = &core.theme;
        let mut lines = Vec::new();
        let ident = [self.fc.vehicle.clone(), self.fc.firmware.clone()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
        if !ident.is_empty() {
            lines.push(kv_text(t, "Vehicle", 9, ident));
        }
        let (None, Some(tel)) = (hidden, &self.tel) else {
            lines.extend(why_line(core, hidden));
            return lines;
        };
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
        if attitude {
            let deg = |v: Option<f64>| {
                v.map_or_else(|| "—".into(), |r| format!("{:+.1}°", r.to_degrees()))
            };
            lines.push(kv_text(
                t,
                "Attitude",
                9,
                format!("roll {}  pitch {}", deg(tel.roll), deg(tel.pitch)),
            ));
        }
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
        lines
    }

    pub(super) fn gps_lines(&self, core: &Core, hidden: Option<&str>) -> Vec<Line<'static>> {
        let t = &core.theme;
        let (None, Some(tel)) = (hidden, &self.tel) else {
            return why_line(core, hidden);
        };
        let fix = tel.fix_type.map(fix_word).unwrap_or_else(|| "—".into());
        let fix_color = match tel.fix_type {
            Some(n) if n >= 3 => t.success(),
            Some(2) => t.warning(),
            _ => t.error(),
        };
        let hdop = tel.eph.or(self.fc.hdop);
        let dash = || "—".to_string();
        let mut l = vec![Line::from(vec![
            Span::styled(format!("{:<9} ", "Fix"), t.dim()),
            Span::styled(fix, Style::default().fg(fix_color)),
            Span::styled("  sats ", t.dim()),
            Span::styled(
                tel.satellites.map_or_else(dash, |n| n.to_string()),
                t.bright(),
            ),
            Span::styled("  HDOP ", t.dim()),
            Span::styled(hdop.map_or_else(dash, |h| format!("{h:.1}")), t.bright()),
        ])];
        match tel.lat.zip(tel.lon) {
            Some((la, lo)) => l.push(kv_text(t, "Position", 9, format!("{la:.6}, {lo:.6}"))),
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

    pub(super) fn sensor_lines(&self, core: &Core, stale: bool, width: u16) -> Vec<Line<'static>> {
        let t = &core.theme;
        if stale {
            return vec![Line::from(Span::styled("readings hidden (stale)", t.dim()))];
        }
        let Some(d) = core.dash() else {
            return Vec::new();
        };
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
        let mut lines = vec![
            kv(t, "Hub", 7, conn(d.hub_connected)),
            kv(t, "FC", 7, conn(d.sensors_fc_connected)),
        ];
        lines.extend(crate::ui::profile_row::packed_lines(t, d, width));
        lines
    }
}
