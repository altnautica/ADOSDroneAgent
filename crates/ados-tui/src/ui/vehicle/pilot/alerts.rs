//! The Pilot block's alert list: the count, then one section per alert group
//! (`alert-groups.json`, in its order) with one line per alert. A readiness
//! item shows its parameters under it (`NAME  current → wanted`, at most six
//! then "… N more") and a dim line naming where to fix it. The terminal
//! writes nothing to the vehicle: the hint points at the surface that does.

use ados_protocol::pilot::{ParamDiff, ReadinessFix};
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::app::Core;
use crate::model::pilot_alerts::{alerts_count_text, group_alerts, PillAlert, Severity};
use crate::model::spec_text::spec_text;
use crate::ui::widgets::kv_text;

use super::super::autonomy::LABEL;
use super::Row;

/// The parameter lines an alert shows before folding the rest.
const MAX_PARAMS: usize = 6;

/// A parameter value as the FC holds it: a whole number without decimals,
/// `—` when unread or absent.
fn have_text(v: Option<f64>) -> String {
    match v {
        Some(v) if v.is_finite() && v.fract() == 0.0 => format!("{}", v as i64),
        Some(v) if v.is_finite() => format!("{v}"),
        _ => "—".into(),
    }
}

/// `NAME  current → wanted`.
fn param_line(p: &ParamDiff) -> String {
    format!("{}  {} → {}", p.name, have_text(p.have), p.want)
}

/// Where the operator fixes the item, since the terminal changes nothing.
fn fix_hint(fix: &ReadinessFix) -> String {
    match fix {
        ReadinessFix::ApplyProfile | ReadinessFix::SetupStep { .. } => {
            "Fix: Aircraft setup, in the cockpit or Mission Control".into()
        }
        ReadinessFix::Settings { section } => format!("Fix: Settings › {section}"),
        ReadinessFix::Parameters { .. } => "Fix: Params tab".into(),
    }
}

/// The detail lines under one alert: its parameters and its fix hint.
fn detail_rows(core: &Core, a: &PillAlert) -> Vec<Row> {
    let t = &core.theme;
    let indent = |text: String, style: Style| -> Row {
        (
            Line::from(vec![Span::raw("      "), Span::styled(text, style)]),
            false,
        )
    };
    let mut out: Vec<Row> = a
        .params
        .iter()
        .take(MAX_PARAMS)
        .map(|p| indent(param_line(p), t.bright()))
        .collect();
    if a.params.len() > MAX_PARAMS {
        let more = a.params.len() - MAX_PARAMS;
        out.push(indent(format!("… {more} more"), t.dim()));
    }
    if let Some(fix) = &a.fix {
        out.push(indent(fix_hint(fix), t.dim()));
    }
    out
}

/// The alert count, then each section's title and its alerts. The alerts are
/// computed now from the readings the tab holds (an aged pilot kind adds its
/// own "not answering" item), so they never read as stale with the pilot
/// kind. The Balance check alert carries its key on the next line (the narrow
/// column never cuts it off) while "I rebalanced it" answers.
pub(super) fn alert_rows(core: &Core, alerts: &[PillAlert], airframe_key: bool) -> Vec<Row> {
    let t = &core.theme;
    let count = alerts_count_text(alerts.len()).unwrap_or_else(|| "–".into());
    let mut out = vec![(kv_text(t, "Alerts", LABEL, count), true)];
    for section in group_alerts(alerts) {
        let title = Line::from(vec![Span::raw("  "), Span::styled(section.title, t.dim())]);
        out.push((title, false));
        for a in &section.alerts {
            let (sym, color) = match a.severity {
                Severity::Critical => (core.sym.err, t.error()),
                Severity::Warning => (core.sym.warn, t.warning()),
            };
            let line = Line::from(vec![
                Span::raw("    "),
                Span::styled(format!("{sym} {}", a.text), Style::default().fg(color)),
            ]);
            out.push((line, false));
            if airframe_key && a.id == "pilot:airframe_balance" {
                let key = Line::from(vec![
                    Span::raw("      "),
                    Span::styled(
                        format!("b {}", spec_text("readiness.airframe_balance.reset", &[])),
                        t.bright(),
                    ),
                ]);
                out.push((key, false));
            }
            out.extend(detail_rows(core, a));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_parameter_reads_whole_or_dash() {
        let p = |have| ParamDiff {
            name: "MAV_GCS_SYSID".into(),
            have,
            want: "1".into(),
        };
        assert_eq!(param_line(&p(Some(255.0))), "MAV_GCS_SYSID  255 → 1");
        assert_eq!(param_line(&p(Some(0.05))), "MAV_GCS_SYSID  0.05 → 1");
        assert_eq!(param_line(&p(None)), "MAV_GCS_SYSID  — → 1");
    }

    #[test]
    fn each_fix_kind_names_where_to_fix_it() {
        let step = ReadinessFix::SetupStep {
            step: "detect".into(),
        };
        let settings = ReadinessFix::Settings {
            section: "sensors".into(),
        };
        let params = ReadinessFix::Parameters {
            names: vec!["FLOW_TYPE".into()],
        };
        let setup = "Fix: Aircraft setup, in the cockpit or Mission Control";
        assert_eq!(fix_hint(&ReadinessFix::ApplyProfile), setup);
        assert_eq!(fix_hint(&step), setup);
        assert_eq!(fix_hint(&settings), "Fix: Settings › sensors");
        assert_eq!(fix_hint(&params), "Fix: Params tab");
    }
}
