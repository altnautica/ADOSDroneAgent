//! The pilot's countdowns as the shared `mission-stall.ts` and the cockpit
//! word them: the persistent mission stall overlay (`No progress to waypoint
//! {wp} for {window_s} s · Skipping in {in_s} s` with Skip waypoint, Return
//! and Land here; `Return blocked · Landing in {in_s} s` with Land here
//! only), the low-battery return countdown, and the pilot's banner in the
//! spec's words. Each stall action names the pilot action it is and the
//! `/api/command` name that reaches it (the route maps `missionskip`, `rtl`
//! and `land` to the pilot's `mission_skip`, `rth` and `land` in ADOS control).

use super::autonomy::{Banner, MissionStall};
use super::spec_text::{has_spec_text, spec_takes_args, spec_text};

/// One action a stall offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StallAction {
    Skip,
    Return,
    Land,
}

impl StallAction {
    /// The `/api/command` name that reaches the pilot action.
    pub fn command(self) -> &'static str {
        match self {
            Self::Skip => "missionskip",
            Self::Return => "rtl",
            Self::Land => "land",
        }
    }

    /// The button's words.
    pub fn label(self) -> String {
        let key = match self {
            Self::Skip => "flow.mission_stall.skip",
            Self::Return => "flow.mission_stall.return",
            Self::Land => "flow.mission_stall.land",
        };
        spec_text(key, &[])
    }

    /// The key that sends it while the stall shows.
    pub fn key(self) -> char {
        match self {
            Self::Skip => 's',
            Self::Return => 'r',
            Self::Land => 'l',
        }
    }
}

/// The stall overlay: its title and its actions, in button order. Always
/// critical (`layout.json` `banners.severity`).
#[derive(Debug, Clone, PartialEq)]
pub struct StallView {
    pub kind: String,
    pub title: String,
    pub actions: Vec<StallAction>,
}

/// Whole seconds, rounded up, never below zero.
fn ceil_s(s: f64) -> String {
    (s.max(0.0).ceil() as i64).to_string()
}

/// The overlay for a stall.
pub fn stall_view(s: &MissionStall) -> StallView {
    let in_s = s.skip_in_s.map(ceil_s);
    if s.kind == "return" {
        let in_s = in_s.unwrap_or_else(|| "—".into());
        return StallView {
            kind: s.kind.clone(),
            title: spec_text("flow.return_blocked.title", &[("in_s", &in_s)]),
            actions: vec![StallAction::Land],
        };
    }
    let wp = s
        .wp_label
        .clone()
        .or_else(|| s.seq.map(|q| (q as i64).to_string()))
        .unwrap_or_else(|| "—".into());
    let window = s
        .window_s
        .map_or_else(|| "—".into(), |w| (w.round() as i64).to_string());
    let title = match &in_s {
        Some(in_s) => spec_text(
            "flow.mission_stall.title",
            &[("wp", &wp), ("window_s", &window), ("in_s", in_s)],
        ),
        None => spec_text(
            "flow.mission_stall.title_hold",
            &[("wp", &wp), ("window_s", &window)],
        ),
    };
    StallView {
        kind: s.kind.clone(),
        title,
        actions: vec![StallAction::Skip, StallAction::Return, StallAction::Land],
    }
}

/// The key hint under the overlay: `s Skip waypoint · r Return · l Land here`.
pub fn stall_keys(v: &StallView) -> String {
    v.actions
        .iter()
        .map(|a| format!("{} {}", a.key(), a.label()))
        .collect::<Vec<_>>()
        .join(" · ")
}

/// The low-battery return countdown, whole seconds rounded up.
pub fn low_battery_line(rth_countdown_s: f64) -> String {
    spec_text(
        "flow.low_battery.title",
        &[("in_s", &ceil_s(rth_countdown_s))],
    )
}

/// A banner in the spec's words while it shows at `now_ms` (epoch ms), else
/// `None`. A `cause` arg reads through `pilot.banner.cause.<cause>` (a dash
/// for one the spec does not name).
pub fn banner_text(b: &Banner, now_ms: u64) -> Option<String> {
    if b.until_ms.is_some_and(|until| now_ms > until) {
        return None;
    }
    let cause = b.args.iter().find(|(k, _)| k == "cause").map(|(_, v)| {
        let key = format!("pilot.banner.cause.{v}");
        if has_spec_text(&key) {
            spec_text(&key, &[])
        } else {
            "—".to_string()
        }
    });
    let args: Vec<(&str, &str)> = b
        .args
        .iter()
        .map(|(k, v)| match (k.as_str(), &cause) {
            ("cause", Some(c)) => (k.as_str(), c.as_str()),
            _ => (k.as_str(), v.as_str()),
        })
        .collect();
    Some(spec_text(&b.text_key, &args))
}

/// The `/api/command` route's refusal (`pilot refused <action>: <code>`) in
/// the spec's words: the code's `refusal.<code>` text, as every surface reads
/// it; a code the spec does not name, or a message of another form, reads
/// as it came.
pub fn pilot_refusal_text(message: &str) -> String {
    let code = message
        .strip_prefix("pilot refused ")
        .and_then(|rest| rest.split_once(": "))
        .map(|(_, code)| code.trim().replace(' ', "_"));
    match code.map(|c| format!("refusal.{c}")) {
        Some(k) if has_spec_text(&k) && !spec_takes_args(&k) => spec_text(&k, &[]),
        _ => message.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::autonomy::Autonomy;
    use serde_json::{json, Value};

    #[test]
    fn a_pilot_refusal_reads_through_the_refusal_table() {
        let cases = [
            (
                "pilot refused mission_skip: no_stall",
                "No stalled waypoint to skip",
            ),
            (
                "pilot refused place_class:confined: armed",
                "Armed; disarm first",
            ),
            (
                "pilot refused launch: route_infeasible",
                "Route too tight for the confined profile",
            ),
            (
                "pilot refused launch: no_prop_radius",
                "Set the propeller tip radius for the confined profile",
            ),
            // A code the table does not name, and a template, read as sent.
            (
                "pilot refused launch: made_up",
                "pilot refused launch: made_up",
            ),
            (
                "pilot refused launch: not_ready",
                "pilot refused launch: not_ready",
            ),
            ("ADOS pilot not reachable", "ADOS pilot not reachable"),
        ];
        for (message, text) in cases {
            assert_eq!(pilot_refusal_text(message), text, "{message}");
        }
    }

    fn golden() -> Value {
        serde_json::from_str(include_str!(
            "../../../ados-control/tests/fixtures/pilot-frame-stall.json"
        ))
        .expect("stall frame fixture is JSON")
    }

    fn pilot_of(frame: Value) -> crate::model::autonomy::Pilot {
        Autonomy::from_value(&json!({"pilot": {"ts_ms": 1, "age_ms": 10, "data": frame}}))
            .pilot
            .expect("a pilot kind")
    }

    #[test]
    fn the_golden_frame_parses_its_stall_banner_and_countdown() {
        let p = pilot_of(golden());
        let s = p.mission_stall.as_ref().expect("a stall");
        assert_eq!(s.kind, "mission");
        assert_eq!(s.seq, Some(3.0));
        assert_eq!(s.wp_label.as_deref(), Some("3"));
        assert_eq!(s.since_ms, Some(1_793_960_000_000));
        assert_eq!(s.skip_in_s, Some(32.5));
        assert_eq!(s.window_s, Some(15.0));
        let b = p.banner.as_ref().expect("a banner");
        assert_eq!(b.text_key, "pilot.banner.mission_stalled");
        assert_eq!(b.until_ms, Some(1_793_960_003_000));
        assert_eq!(p.rth_countdown_s, None);
    }

    #[test]
    fn the_golden_stall_reads_as_the_spec_with_three_actions() {
        let p = pilot_of(golden());
        let v = stall_view(p.mission_stall.as_ref().unwrap());
        assert_eq!(
            v.title,
            "No progress to waypoint 3 for 15 s · Skipping in 33 s"
        );
        assert_eq!(
            v.actions,
            vec![StallAction::Skip, StallAction::Return, StallAction::Land]
        );
        assert_eq!(stall_keys(&v), "s Skip waypoint · r Return · l Land here");
        let cmds: Vec<_> = v.actions.iter().map(|a| a.command()).collect();
        assert_eq!(cmds, ["missionskip", "rtl", "land"]);
    }

    #[test]
    fn a_stall_without_a_countdown_holds_for_the_operator() {
        let mut f = golden();
        f["mission_stall"]["skip_in_s"] = Value::Null;
        f["mission_stall"]["window_s"] = json!(14.6);
        let v = stall_view(pilot_of(f).mission_stall.as_ref().unwrap());
        assert_eq!(
            v.title,
            "No progress to waypoint 3 for 15 s · Holding for you"
        );
    }

    #[test]
    fn a_blocked_return_offers_land_here_only() {
        let mut f = golden();
        f["mission_stall"] = json!({"kind": "return", "seq": null, "wp_label": null, "since_ms": 1, "skip_in_s": 44.1, "window_s": 20.0});
        let v = stall_view(pilot_of(f).mission_stall.as_ref().unwrap());
        assert_eq!(v.title, "Return blocked · Landing in 45 s");
        assert_eq!(v.actions, vec![StallAction::Land]);
        assert_eq!(stall_keys(&v), "l Land here");
    }

    #[test]
    fn an_unknown_kind_is_no_stall() {
        let mut f = golden();
        f["mission_stall"]["kind"] = json!("orbit");
        assert!(pilot_of(f).mission_stall.is_none());
    }

    #[test]
    fn the_banner_reads_the_spec_while_its_slot_lasts() {
        let p = pilot_of(golden());
        let b = p.banner.as_ref().unwrap();
        assert_eq!(
            banner_text(b, 1_793_960_001_000).as_deref(),
            Some("No progress to waypoint 3 · Holding · Skipping in 33 s")
        );
        assert_eq!(banner_text(b, 1_793_960_003_001), None);
        let cause = Banner {
            text_key: "pilot.banner.fc_mode_change".into(),
            args: vec![("mode".into(), "RTL".into()), ("cause".into(), "fc".into())],
            until_ms: None,
        };
        assert_eq!(
            banner_text(&cause, 0).as_deref(),
            Some("FC switched to RTL: flight controller")
        );
    }

    #[test]
    fn the_low_battery_countdown_rounds_up() {
        assert_eq!(
            low_battery_line(9.2),
            "Battery reserve reached · Returning along the flown path in 10 s"
        );
        assert_eq!(
            low_battery_line(-1.0),
            "Battery reserve reached · Returning along the flown path in 0 s"
        );
    }
}
