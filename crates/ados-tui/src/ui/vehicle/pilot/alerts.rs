//! The Pilot block's alert list: the count, then one section per alert group
//! (`alert-groups.json`, in its order) with one line per alert. A readiness
//! item shows its parameters under it (`NAME  current → wanted`, at most six
//! then "… N more") and a dim line naming where to fix it. The terminal
//! writes nothing to the vehicle: the hint points at the surface that does.

use ados_protocol::pilot::{ParamDiff, ReadinessFix};
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::app::Core;
use crate::model::pilot_alerts::{
    alert_title, alerts_count_text, group_alerts, PillAlert, Severity,
};
use crate::model::spec_text::{has_spec_text, spec_text};
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

/// A cockpit settings section's title (`cockpit.settingsSections.<id>.title`
/// in the shared locale); an unknown id reads as itself.
fn section_title(id: &str) -> &str {
    match id {
        "profile" => "Profile",
        "region" => "Region",
        "battery" => "Battery",
        "wifi" => "Wi-Fi",
        "hotspot" => "Hotspot",
        "uplink" => "Uplink",
        "cellular" => "Cellular",
        "macPin" => "MAC pin",
        "cloud" => "Cloud",
        "selfHeal" => "Self-heal",
        "mavlink" => "MAVLink",
        "security" => "Security",
        "vision" => "Vision",
        "camera" => "Camera",
        "capture" => "Capture",
        "sensors" => "Sensors",
        "swarm" => "Swarm",
        "discovery" => "Discovery",
        "display" => "Display",
        "advanced" => "Advanced",
        other => other,
    }
}

/// Setup is written from these surfaces; the terminal changes nothing.
const SETUP_WHERE: &str = "in the cockpit or Mission Control";

/// Where the operator fixes the item: one line, and for Aircraft setup a
/// second naming the surfaces that run it, so a narrow column cuts neither.
/// A setup step reads its spec title (`setup.<step>.title`).
fn fix_hint(fix: &ReadinessFix) -> Vec<String> {
    let setup = |at: Option<String>| {
        let line = match at {
            Some(at) => format!("Fix: Aircraft setup › {at}"),
            None => "Fix: Aircraft setup".into(),
        };
        vec![line, SETUP_WHERE.into()]
    };
    match fix {
        ReadinessFix::ApplyProfile => setup(Some(spec_text("setup.card.safety.title", &[]))),
        ReadinessFix::SetupStep { step } => {
            let key = format!("setup.{step}.title");
            setup(has_spec_text(&key).then(|| spec_text(&key, &[])))
        }
        ReadinessFix::Settings { section } => {
            vec![format!("Fix: Settings › {}", section_title(section))]
        }
        ReadinessFix::Parameters { .. } => vec!["Fix: Params tab".into()],
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
        out.extend(fix_hint(fix).into_iter().map(|l| indent(l, t.dim())));
    }
    out
}

/// The alert count, then each section's title and its alerts. The alerts are
/// computed now from the readings the tab holds (an aged pilot kind adds its
/// own "not answering" item), so they never read as stale with the pilot
/// kind. The Balance check alert carries its key on the next line (the narrow
/// column never cuts it off) while "I rebalanced it" answers.
pub fn alert_rows(core: &Core, alerts: &[PillAlert], airframe_key: bool) -> Vec<Row> {
    let t = &core.theme;
    let count = alerts_count_text(alerts).unwrap_or_else(|| "–".into());
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
                Span::styled(
                    format!("{sym} {}", alert_title(a)),
                    Style::default().fg(color),
                ),
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
        let step = |s: &str| ReadinessFix::SetupStep { step: s.into() };
        let settings = |s: &str| ReadinessFix::Settings { section: s.into() };
        let params = ReadinessFix::Parameters {
            names: vec!["FLOW_TYPE".into()],
        };
        let hint = |f: &ReadinessFix| fix_hint(f).join(" / ");
        let where_ = "in the cockpit or Mission Control";
        assert_eq!(
            hint(&ReadinessFix::ApplyProfile),
            format!("Fix: Aircraft setup › Safety / {where_}")
        );
        assert_eq!(
            hint(&step("frame")),
            format!("Fix: Aircraft setup › Frame / {where_}")
        );
        assert_eq!(
            hint(&step("no_such_step")),
            format!("Fix: Aircraft setup / {where_}")
        );
        assert_eq!(hint(&settings("sensors")), "Fix: Settings › Sensors");
        assert_eq!(hint(&settings("macPin")), "Fix: Settings › MAC pin");
        assert_eq!(hint(&settings("later")), "Fix: Settings › later");
        assert_eq!(hint(&params), "Fix: Params tab");
    }
}
