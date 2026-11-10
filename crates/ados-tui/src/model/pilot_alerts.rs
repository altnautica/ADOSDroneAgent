//! The alerts the cockpit's status pill draws from the pilot kind and the
//! flight controller: failing readiness items, the FC's PreArm/Arm texts and
//! its status texts at WARNING or worse. The same rules, texts and order as
//! the shared `status-pill.ts` `pillAlerts` for these sources.

use super::autonomy::{FcStatusText, ReadinessItem};
use super::spec_text::spec_text;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Critical,
    Warning,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PillAlert {
    pub severity: Severity,
    pub text: String,
}

/// A PreArm/Arm line without its prefix, or `None` for any other text.
fn prearm_body(text: &str) -> Option<&str> {
    ["PreArm:", "Arm:"]
        .iter()
        .find_map(|p| text.strip_prefix(p))
        .map(str::trim_start)
}

/// Every active alert, critical first, then in source order: readiness,
/// the newest PreArm/Arm text, the status texts (one per text, at the worst
/// severity it was sent with).
pub fn pilot_alerts(
    readiness: &[ReadinessItem],
    prearm: Option<&[String]>,
    fc_status: &[FcStatusText],
) -> Vec<PillAlert> {
    let mut out = Vec::new();
    for r in readiness.iter().filter(|r| !r.ok) {
        // A blocking item refuses take-off: critical, as the pre-flight sheet fails it.
        out.push(PillAlert {
            severity: if r.block {
                Severity::Critical
            } else {
                Severity::Warning
            },
            text: if r.msg.is_empty() {
                r.id.clone()
            } else {
                r.msg.clone()
            },
        });
    }
    if let Some(last) = prearm.and_then(<[String]>::last) {
        let text = prearm_body(last).unwrap_or(last);
        out.push(PillAlert {
            severity: Severity::Warning,
            text: spec_text("flow.preflight.fc_prearm.failing", &[("text", text)]),
        });
    }
    let fc_start = out.len();
    for s in fc_status {
        // PreArm/Arm lines already arrive through `prearm`.
        if prearm_body(&s.text).is_some() {
            continue;
        }
        let severity = if s.severity <= 2.0 {
            Severity::Critical
        } else {
            Severity::Warning
        };
        match out[fc_start..].iter_mut().find(|a| a.text == s.text) {
            Some(seen) if severity == Severity::Critical => seen.severity = severity,
            Some(_) => {}
            None => out.push(PillAlert {
                severity,
                text: s.text.clone(),
            }),
        }
    }
    // A stable sort: equal severities keep their source order.
    out.sort_by_key(|a| a.severity != Severity::Critical);
    out
}

/// The count of active alerts as the pill words it (`1 warning`,
/// `3 warnings`); `None` with none.
pub fn alerts_count_text(n: usize) -> Option<String> {
    match n {
        0 => None,
        1 => Some(spec_text("cockpit.status.one", &[])),
        n => Some(spec_text("cockpit.status.many", &[("n", &n.to_string())])),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, ok: bool, block: bool, msg: &str) -> ReadinessItem {
        ReadinessItem {
            id: id.into(),
            ok,
            block,
            msg: msg.into(),
        }
    }

    fn st(severity: f64, text: &str) -> FcStatusText {
        FcStatusText {
            severity,
            text: text.into(),
        }
    }

    #[test]
    fn readiness_prearm_and_status_texts_in_pill_order() {
        let alerts = pilot_alerts(
            &[
                item("agl", false, false, "No rangefinder"),
                item("position", true, true, "Position ok"),
                item("fc_profile", false, true, ""),
            ],
            Some(&["PreArm: Old".into(), "Arm: Throttle too high".into()]),
            &[
                st(4.0, "EKF variance"),
                st(4.0, "PreArm: Compass not calibrated"),
                st(2.0, "EKF variance"),
                st(4.0, "Low disk"),
            ],
        );
        let got: Vec<(Severity, &str)> = alerts
            .iter()
            .map(|a| (a.severity, a.text.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                (Severity::Critical, "fc_profile"),
                (Severity::Critical, "EKF variance"),
                (Severity::Warning, "No rangefinder"),
                (Severity::Warning, "Flight controller: Throttle too high"),
                (Severity::Warning, "Low disk"),
            ]
        );
    }

    #[test]
    fn an_unknown_prearm_list_and_an_empty_one_add_nothing() {
        assert!(pilot_alerts(&[], None, &[]).is_empty());
        assert!(pilot_alerts(&[], Some(&[]), &[]).is_empty());
    }

    #[test]
    fn the_count_reads_the_spec() {
        assert_eq!(alerts_count_text(0), None);
        assert_eq!(alerts_count_text(1).as_deref(), Some("1 warning"));
        assert_eq!(alerts_count_text(3).as_deref(), Some("3 warnings"));
    }
}
