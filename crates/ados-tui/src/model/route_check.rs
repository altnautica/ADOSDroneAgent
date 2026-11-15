//! The confined-place profile as the shared `route-check.ts` words it: the
//! place class (`place_class`, absent reads open), the pilot's route check of
//! the loaded mission (`route_check`, with its tightest point), the speed-cap
//! line by its reason, the `/api/command` names that set the class, and which
//! pilot banners the stall overlay hides while it shows (`mission-stall.ts`
//! `stallShadowsBanner`).

use serde_json::Value;

use super::autonomy::MissionStall;
use super::spec_text::{has_spec_text, spec_text};

const NO_DATA: &str = "—";

/// The two place classes.
pub const PLACE_CLASSES: [&str; 2] = ["open", "confined"];

/// The tightest checked point of the route.
#[derive(Debug, Clone, PartialEq)]
pub struct RoutePinch {
    /// The leg runs from mission item `from_seq` to `to_seq`.
    pub from_seq: i64,
    pub to_seq: i64,
    /// Local NED, metres.
    pub ned: [f64; 3],
    pub clearance_m: f64,
    pub required_m: f64,
}

/// The pilot's route check, present while confined with a mission loaded.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteCheck {
    /// `feasible`, `unverified` or `infeasible`.
    pub state: String,
    pub required_m: Option<f64>,
    pub margin_m: Option<f64>,
    pub sigma_m: Option<f64>,
    pub pinch: Option<RoutePinch>,
    /// `to_seq` of the legs through cells not yet observed.
    pub unverified_legs: Vec<i64>,
}

fn num(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64).filter(|x| x.is_finite())
}

fn pinch(v: &Value) -> Option<RoutePinch> {
    let ned = v.get("ned")?.as_array().filter(|a| a.len() >= 3)?;
    let at = |i: usize| ned[i].as_f64().filter(|x| x.is_finite());
    Some(RoutePinch {
        from_seq: v.get("from_seq")?.as_i64()?,
        to_seq: v.get("to_seq")?.as_i64()?,
        ned: [at(0)?, at(1)?, at(2)?],
        clearance_m: num(v, "clearance_m")?,
        required_m: num(v, "required_m")?,
    })
}

/// `route_check` of a known state, else `None`.
pub fn parse_route_check(v: &Value) -> Option<RouteCheck> {
    let state = v
        .get("state")?
        .as_str()
        .filter(|s| ["feasible", "unverified", "infeasible"].contains(s))?;
    Some(RouteCheck {
        state: state.to_string(),
        required_m: num(v, "required_m"),
        margin_m: num(v, "margin_m"),
        sigma_m: num(v, "sigma_m"),
        pinch: v.get("pinch").and_then(pinch),
        unverified_legs: v
            .get("unverified_legs")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_i64).collect())
            .unwrap_or_default(),
    })
}

/// `place_class` of the pilot frame; absent or unknown reads open.
pub fn parse_place_class(d: &Value) -> String {
    d.get("place_class")
        .and_then(Value::as_str)
        .filter(|c| PLACE_CLASSES.contains(c))
        .unwrap_or("open")
        .to_string()
}

/// Clearances and required distances: two decimals ("0.58").
fn metres2(v: Option<f64>) -> String {
    v.map_or_else(
        || NO_DATA.to_string(),
        |v| format!("{:.2}", (v * 100.0).round() / 100.0),
    )
}

/// A position component: one decimal, halves away from zero, never "-0.0".
fn metres1(v: f64) -> String {
    let r = (v * 10.0).round();
    if r == 0.0 {
        "0.0".to_string()
    } else {
        format!("{:.1}", r / 10.0)
    }
}

/// The report's weight: `ok` feasible, `warn` unverified, `critical`
/// infeasible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportSeverity {
    Ok,
    Warn,
    Critical,
}

/// The route-check report in the spec's words.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteCheckReport {
    pub severity: ReportSeverity,
    pub title: String,
    /// The state line (`route_check.<state>`).
    pub line: String,
    /// Where the tightest checked point is (`route_check.pinch_at`).
    pub pinch_at: Option<String>,
}

/// The report for a route check: the infeasible line names the pinch leg
/// (`3→4`) with its clearance and the clearance it needs.
pub fn route_check_report(rc: &RouteCheck) -> RouteCheckReport {
    let p = rc.pinch.as_ref();
    let key = format!("route_check.{}", rc.state);
    let line = if rc.state == "infeasible" {
        let leg = p.map_or_else(
            || NO_DATA.to_string(),
            |p| format!("{}→{}", p.from_seq, p.to_seq),
        );
        let clearance = metres2(p.map(|p| p.clearance_m));
        let required = metres2(p.map(|p| p.required_m).or(rc.required_m));
        spec_text(
            &key,
            &[
                ("leg", &leg),
                ("clearance_m", &clearance),
                ("required_m", &required),
            ],
        )
    } else {
        spec_text(&key, &[("required_m", &metres2(rc.required_m))])
    };
    let pinch_at = p.map(|p| {
        let (n, e, h) = (metres1(p.ned[0]), metres1(p.ned[1]), metres1(-p.ned[2]));
        spec_text("route_check.pinch_at", &[("n", &n), ("e", &e), ("h", &h)])
    });
    RouteCheckReport {
        severity: match rc.state.as_str() {
            "infeasible" => ReportSeverity::Critical,
            "unverified" => ReportSeverity::Warn,
            _ => ReportSeverity::Ok,
        },
        title: spec_text("route_check.title", &[]),
        line,
        pinch_at,
    }
}

/// The "Confined" chip text while the class is confined, else `None`.
pub fn confined_chip_text(place_class: &str) -> Option<String> {
    (place_class == "confined").then(|| spec_text("chip.confined", &[]))
}

/// The speed-cap line by its reason (`pilot.speed_cap.<reason>`, dashes as
/// underscores), or `None` for a reason the spec does not name.
pub fn speed_cap_line(reason: &str, v_h_mps: f64) -> Option<String> {
    let key = format!("pilot.speed_cap.{}", reason.replace('-', "_"));
    let v = format!("{}", (v_h_mps * 10.0).round() / 10.0);
    has_spec_text(&key).then(|| spec_text(&key, &[("v", &v)]))
}

/// The class the place key switches to from `current`.
pub fn other_place_class(current: &str) -> &'static str {
    if current == "confined" {
        "open"
    } else {
        "confined"
    }
}

/// The `/api/command` name that sets a place class.
pub fn place_command(class: &str) -> String {
    format!("placeclass_{class}")
}

/// A place class's picker label (`flow.launch.place_class.<class>`).
pub fn place_label(class: &str) -> String {
    spec_text(&format!("flow.launch.place_class.{class}"), &[])
}

/// The pilot banners a shown stall overlay repeats: hidden while it shows.
const SHADOWED_BANNERS: [&str; 3] = [
    "pilot.banner.mission_stalled",
    "pilot.banner.mission_stalled_hold",
    "pilot.banner.return_blocked",
];

/// Whether the banner `text_key` hides behind the stall overlay.
pub fn stall_shadows_banner(text_key: &str, stall: Option<&MissionStall>) -> bool {
    stall.is_some() && SHADOWED_BANNERS.contains(&text_key)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const CONFINED: &str =
        include_str!("../../../ados-control/tests/fixtures/pilot-frame-confined.json");

    fn golden() -> Value {
        serde_json::from_str(CONFINED).expect("confined frame fixture is JSON")
    }

    #[test]
    fn the_golden_frame_reads_its_class_and_route_check() {
        let d = golden();
        assert_eq!(parse_place_class(&d), "confined");
        let rc = parse_route_check(&d["route_check"]).expect("route check");
        assert_eq!(rc.state, "infeasible");
        assert_eq!(rc.required_m, Some(0.577));
        assert_eq!(rc.unverified_legs, vec![5, 6]);
        let p = rc.pinch.as_ref().expect("pinch");
        assert_eq!((p.from_seq, p.to_seq, p.ned), (3, 4, [41.25, -17.5, -2.0]));
        assert_eq!(parse_place_class(&json!({})), "open");
        assert_eq!(parse_route_check(&json!({"state": "maybe"})), None);
    }

    #[test]
    fn the_report_reads_as_every_surface_words_it() {
        let rc = parse_route_check(&golden()["route_check"]).unwrap();
        let r = route_check_report(&rc);
        assert_eq!(r.severity, ReportSeverity::Critical);
        assert_eq!(r.title, "Route check");
        assert_eq!(r.line, "Route too tight: leg 3→4 has 0.52 m, needs 0.58 m");
        assert_eq!(
            r.pinch_at.as_deref(),
            Some("At N 41.3 m, E -17.5 m, height 2.0 m")
        );
        let clear = RouteCheck {
            state: "feasible".into(),
            pinch: None,
            ..rc.clone()
        };
        assert_eq!(
            route_check_report(&clear).line,
            "Route clear · keeps 0.58 m"
        );
        let unseen = RouteCheck {
            state: "unverified".into(),
            ..rc
        };
        assert_eq!(
            route_check_report(&unseen).line,
            "Route not seen yet · slow near walls"
        );
        assert_eq!(route_check_report(&unseen).severity, ReportSeverity::Warn);
    }

    #[test]
    fn the_cap_chip_and_commands_read_the_spec() {
        assert_eq!(
            speed_cap_line("confined-near", 0.5).as_deref(),
            Some("Near the clearance limit: 0.5 m/s max")
        );
        assert_eq!(speed_cap_line("made-up", 1.0), None);
        assert_eq!(confined_chip_text("confined").as_deref(), Some("Confined"));
        assert_eq!(confined_chip_text("open"), None);
        assert_eq!(
            place_command(other_place_class("open")),
            "placeclass_confined"
        );
        assert_eq!(
            place_command(other_place_class("confined")),
            "placeclass_open"
        );
        assert_eq!(place_label("confined"), "Confined space");
    }

    #[test]
    fn the_stall_hides_only_its_own_banners() {
        let stall = MissionStall {
            kind: "mission".into(),
            seq: Some(3.0),
            wp_label: None,
            since_ms: None,
            skip_in_s: None,
            window_s: None,
        };
        for key in SHADOWED_BANNERS {
            assert!(stall_shadows_banner(key, Some(&stall)));
            assert!(!stall_shadows_banner(key, None));
        }
        assert!(!stall_shadows_banner(
            "pilot.banner.route_infeasible",
            Some(&stall)
        ));
    }
}
