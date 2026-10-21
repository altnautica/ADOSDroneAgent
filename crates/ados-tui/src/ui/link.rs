//! Tab 3, Link: the WFB radio card and 5-minute trends, the LoRa lane, and
//! the ground station's adapters, receiver and mesh.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use serde_json::Value;

use crate::app::{Core, FrameOut, Tab};
use crate::keymap::{Command, Ctx};
use crate::model::link::{
    feed_adapters, feed_combined, feed_lora, feed_relays, Counters, LinkHistory, Wfb,
};
use crate::poll::{ResourceId, Sub};
use crate::ui::vehicle::chart::{line_chart, Trace};
use crate::ui::widgets::{chip, kv, kv_text, panel, render_cells, Cell};

mod tables;

#[derive(Default)]
pub struct LinkTab {
    wfb: Option<Wfb>,
    hist: LinkHistory,
    counters: Counters,
}

/// `—` for a missing value.
pub(super) fn or_dash<T>(v: Option<T>, f: impl Fn(T) -> String) -> String {
    v.map(f).unwrap_or_else(|| "—".into())
}

fn state_color(core: &Core, state: &str) -> Color {
    let t = &core.theme;
    match state {
        "connected" | "active" => t.success(),
        "degraded" | "searching" | "connecting" | "binding" | "auto_pairing" | "rf_unverified" => {
            t.warning()
        }
        "stale" | "disabled" | "unknown" => t.muted(),
        _ => t.error(),
    }
}

impl LinkTab {
    fn ground(core: &Core) -> bool {
        core.profile() == "ground_station"
    }

    fn card(&self, f: &mut Frame, area: Rect, core: &Core) {
        let t = &core.theme;
        let block = panel(t, core.sym, "WFB radio");
        let inner = block.inner(area);
        f.render_widget(block, area);
        let Some(w) = &self.wfb else {
            let msg = core
                .slot(&ResourceId::Wfb)
                .and_then(|s| s.error.clone())
                .unwrap_or_else(|| "Loading…".into());
            f.render_widget(Paragraph::new(Span::styled(msg, t.dim())), inner);
            return;
        };
        const W: usize = 9;
        let mut state = vec![
            Span::styled(format!("{:<W$} ", "State"), t.dim()),
            chip(
                t,
                &w.state.replace('_', " ").to_uppercase(),
                state_color(core, &w.state),
            ),
        ];
        if let Some(d) = &w.link_diag {
            state.push(Span::styled(format!(" {d}"), t.dim()));
        }
        let mut lines = vec![Line::from(state)];
        let adapter = [w.interface.clone(), w.chipset.clone(), w.driver.clone()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
        lines.push(kv_text(
            t,
            "Adapter",
            W,
            if adapter.is_empty() {
                "no adapter".into()
            } else {
                adapter
            },
        ));
        let actual = match (w.actual_channel, w.channel) {
            (Some(a), Some(c)) if a != c => format!(" (home {c})"),
            _ => String::new(),
        };
        lines.push(kv_text(
            t,
            "Channel",
            W,
            format!(
                "{}{actual} · {} · {}",
                or_dash(w.actual_channel.or(w.channel), |c| c.to_string()),
                or_dash(w.frequency_mhz, |f| format!("{f} MHz")),
                or_dash(w.bandwidth_mhz, |b| format!("{b} MHz wide"))
            ),
        ));
        lines.push(kv_text(
            t,
            "MCS / TX",
            W,
            format!(
                "MCS {} · {} of {} dBm",
                or_dash(w.mcs_index, |m| m.to_string()),
                or_dash(w.tx_power_dbm, |p| format!("{p:.0}")),
                or_dash(w.tx_power_max_dbm, |p| format!("{p:.0}"))
            ),
        ));
        lines.push(kv_text(
            t,
            "Region",
            W,
            w.regulatory_domain.clone().unwrap_or_else(|| "—".into()),
        ));
        lines.push(kv_text(
            t,
            "Signal",
            W,
            format!(
                "RSSI {} · SNR {} · noise {}",
                or_dash(w.rssi_dbm, |v| format!("{v:.0} dBm")),
                or_dash(w.snr_db, |v| format!("{v:.0} dB")),
                or_dash(w.noise_dbm, |v| format!("{v:.0}"))
            ),
        ));
        lines.push(kv_text(
            t,
            "Traffic",
            W,
            format!(
                "loss {} · {}",
                or_dash(w.loss_percent, |v| format!("{v:.1} %")),
                or_dash(w.bitrate_kbps, |v| format!("{:.2} Mbit/s", v / 1000.0))
            ),
        ));
        let fec_failed_style = match w.fec_failed {
            Some(n) if n > 0 => Style::default().fg(t.warning()),
            _ => t.bright(),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{:<W$} ", "FEC"), t.dim()),
            Span::styled(
                format!("recovered {}", or_dash(w.fec_recovered, |n| n.to_string())),
                t.bright(),
            ),
            Span::styled(" · ", t.dim()),
            Span::styled(
                format!("failed {}", or_dash(w.fec_failed, |n| n.to_string())),
                fec_failed_style,
            ),
            Span::styled(" per 1 s", t.dim()),
        ]));
        if let Some(r) = w.restart_count.filter(|_| !w.ground) {
            lines.push(kv(
                t,
                "Restarts",
                W,
                Span::styled(
                    r.to_string(),
                    if r > 0 {
                        Style::default().fg(t.warning())
                    } else {
                        t.bright()
                    },
                ),
            ));
        }
        f.render_widget(Paragraph::new(lines), inner);
    }

    fn trends(&self, f: &mut Frame, area: Rect, core: &Core) {
        let t = &core.theme;
        let rows = Layout::vertical([
            Constraint::Min(5),
            Constraint::Length(3),
            Constraint::Length(3),
        ])
        .split(area);
        line_chart(
            f,
            rows[0],
            core,
            "RSSI dBm · SNR dB",
            "-5m",
            &[
                Trace {
                    name: "RSSI",
                    ring: &self.hist.rssi,
                    color: t.accent(),
                },
                Trace {
                    name: "SNR",
                    ring: &self.hist.snr,
                    color: t.info(),
                },
            ],
        );
        let spark = |f: &mut Frame, rect: Rect, title: String, data: Vec<u64>, color: Color| {
            let block = panel(t, core.sym, &title);
            let inner = block.inner(rect);
            f.render_widget(block, rect);
            render_cells(
                f,
                t,
                inner,
                vec![Cell::Spark {
                    data,
                    max: None,
                    color,
                }],
            );
        };
        spark(
            f,
            rows[1],
            format!(
                "Loss {}",
                or_dash(self.hist.loss.last(), |v| format!("{v:.1} %"))
            ),
            self.hist.loss.spark(10.0),
            t.warning(),
        );
        spark(
            f,
            rows[2],
            format!(
                "Bitrate {}",
                or_dash(self.hist.bitrate.last(), |v| format!(
                    "{:.2} Mbit/s",
                    v / 1000.0
                ))
            ),
            self.hist.bitrate.spark(1.0),
            t.success(),
        );
    }
}

impl Tab for LinkTab {
    fn ctx(&self, _core: &Core) -> Vec<Ctx> {
        Vec::new()
    }

    fn subs(&self, core: &Core) -> Vec<Sub> {
        let mut subs = vec![
            Sub::every_ms(ResourceId::Wfb, "/api/wfb", 2000),
            Sub::every_ms(ResourceId::Lora, "/api/v1/lora", 2000),
            Sub::every_ms(ResourceId::LoraBeacons, "/api/v1/lora/beacons", 5000),
        ];
        if Self::ground(core) {
            subs.push(Sub::every_ms(
                ResourceId::Mesh,
                "/api/v1/ground-station/mesh",
                2000,
            ));
            subs.push(Sub::every_ms(
                ResourceId::ReceiverCombined,
                "/api/v1/ground-station/wfb/receiver/combined",
                2000,
            ));
            subs.push(Sub::every_ms(
                ResourceId::ReceiverRelays,
                "/api/v1/ground-station/wfb/receiver/relays",
                2000,
            ));
        }
        subs
    }

    fn on_deactivate(&mut self, _core: &mut Core) {
        // Polling stops while the tab is hidden; a baseline kept across the gap
        // would make the first delta after returning cover the whole absence.
        self.counters = Counters::default();
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
            ResourceId::Wfb => {
                let w = Wfb::from_value(v);
                self.hist.record(&w);
                feed_adapters(&mut self.counters, v);
                self.wfb = Some(w);
            }
            ResourceId::Lora => feed_lora(&mut self.counters, v),
            ResourceId::ReceiverCombined => feed_combined(&mut self.counters, v),
            ResourceId::ReceiverRelays => feed_relays(&mut self.counters, v),
            _ => {}
        }
    }

    fn on_command(&mut self, _core: &mut Core, _cmd: Command) -> bool {
        false
    }

    fn render(&mut self, f: &mut Frame, area: Rect, core: &Core, _out: &mut FrameOut) {
        let lora_present = core
            .value(&ResourceId::Lora)
            .and_then(|v| v.get("present"))
            .and_then(Value::as_bool)
            == Some(true);
        let ground = Self::ground(core);
        let mut constraints = vec![Constraint::Length(12)];
        if ground {
            constraints.push(Constraint::Min(8));
        }
        if lora_present {
            constraints.push(Constraint::Min(7));
        }
        constraints.push(Constraint::Length(0));
        let rows = Layout::vertical(constraints).split(area);
        let top = Layout::horizontal([Constraint::Length(50), Constraint::Min(30)]).split(rows[0]);
        self.card(f, top[0], core);
        self.trends(f, top[1], core);
        let mut next = 1;
        if ground {
            tables::ground(f, rows[next], core, &self.counters);
            next += 1;
        }
        if lora_present {
            tables::lora(f, rows[next], core, &self.counters);
        }
    }

    fn context_line(&self, core: &Core) -> Option<String> {
        let mut s =
            "FEC and loss are per 1 s radio interval · trends: last 5 min at 2 s".to_string();
        if Self::ground(core) {
            s.push_str(" · Δ = change since the last 2 s poll");
        }
        Some(s)
    }
}

#[cfg(test)]
mod tests;
