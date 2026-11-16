//! Controller feedback from the drone's `pilot` state: the lightbar state and
//! colour (spec priority order, the profile's colour while the sticks fly),
//! the rumble cue the spec's `feedback` rules give a state change, and the
//! rumble pulse scheduler.

use serde_json::Value;

use super::gestures::Cue;
use super::spec::{ds4, FeedbackRule, Trigger};

/// The `pilot` state is stale (lost link) after this long.
pub const STATE_STALE_MS: u64 = 2000;

/// What the ground station knows about the drone's pilot right now.
#[derive(Debug, Clone, Default)]
pub struct PilotView {
    /// The last `pilot` state object and when it arrived (monotonic ms).
    pub state: Option<(Value, u64)>,
    /// Whether the GS relay socket is connected.
    pub relay_up: bool,
}

/// RTL-family custom modes per vehicle class (Copter RTL 6 / SMART_RTL 21;
/// Plane RTL 11 / QRTL 21).
pub fn is_rth_mode(vehicle_class: &str, fc_mode: i64) -> bool {
    match vehicle_class {
        "fixedwing" | "vtol" => matches!(fc_mode, 11 | 21),
        _ => matches!(fc_mode, 6 | 21),
    }
}

impl PilotView {
    /// The live state, or `None` when the relay is down or the state is stale.
    pub fn live(&self, now_ms: u64) -> Option<&Value> {
        if !self.relay_up {
            return None;
        }
        self.state
            .as_ref()
            .filter(|(_, at)| now_ms.saturating_sub(*at) < STATE_STALE_MS)
            .map(|(v, _)| v)
    }

    pub fn rth_active(&self, now_ms: u64) -> bool {
        self.live(now_ms).is_some_and(|s| {
            s["owner"] == "fc_mode"
                && is_rth_mode(
                    s["vehicle_class"].as_str().unwrap_or("multirotor"),
                    s["fc_mode"].as_i64().unwrap_or(-1),
                )
        })
    }

    /// Whether the vehicle is known to be on the ground. Unknown (no live
    /// state, or `landed: null` while the pilot cannot hear the FC) counts as
    /// possibly airborne, so the gestures offer Land rather than Take off.
    pub fn known_landed(&self, now_ms: u64) -> bool {
        self.live(now_ms).is_some_and(|s| s["landed"] == true)
    }

    /// The spec lightbar state id, first match in the spec priority order.
    pub fn lightbar_state(&self, now_ms: u64) -> &'static str {
        let Some(s) = self.live(now_ms) else {
            return "failsafe";
        };
        if !s["link_loss_in_s"].is_null()
            || matches!(
                s["hold_reason"].as_str(),
                Some("controller-lost" | "input-stale")
            )
        {
            return "failsafe";
        }
        if !s["rth_countdown_s"].is_null() {
            return "low_battery";
        }
        if self.rth_active(now_ms) {
            return "rth";
        }
        match s["owner"].as_str().unwrap_or("idle") {
            "mission_fc" => "mission",
            "skill" => "skill",
            "pilot" => s["profile"]
                .as_str()
                .and_then(|p| ds4().lightbar.profile_states.get(p))
                .map_or("pilot", String::as_str),
            "goto" | "swarm" | "tuning" => "pilot",
            // Unknown (`armed: null`) may be flying: never the disarmed light.
            _ if s["armed"] != false => "armed_idle",
            _ => "disarmed",
        }
    }
}

/// An RGB colour with brightness 0..1 for one instant of a lightbar state.
pub fn lightbar_rgb(state: &str, now_ms: u64) -> [u8; 3] {
    let Some(st) = ds4().lightbar.states.get(state) else {
        return [0, 0, 0];
    };
    let base = parse_hex(&st.color);
    let hz = st.hz.unwrap_or(0.0);
    let level = match st.pattern.as_str() {
        "blink" if hz > 0.0 => {
            let period = 1000.0 / hz;
            if (now_ms as f32 % period) < period / 2.0 {
                1.0
            } else {
                0.0
            }
        }
        "pulse" if hz > 0.0 => {
            let phase = (now_ms as f32 / 1000.0) * hz * std::f32::consts::TAU;
            0.15 + 0.85 * (0.5 - 0.5 * phase.cos())
        }
        _ => 1.0,
    };
    base.map(|c| (f32::from(c) * level).round() as u8)
}

fn parse_hex(s: &str) -> [u8; 3] {
    let h = s.trim_start_matches('#');
    let byte = |i: usize| {
        h.get(i..i + 2)
            .and_then(|x| u8::from_str_radix(x, 16).ok())
            .unwrap_or(0)
    };
    [byte(0), byte(2), byte(4)]
}

/// The rumble class a state transition earns, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alert {
    ModeChange,
    Warning,
    Critical,
}

/// The alert a pilot-state update earns under the spec's `feedback` rules:
/// the strongest class with a rule that fires, or `None`. `None` for `next`
/// is a lost or stale state; `None` for `prev` is a first state, which earns
/// nothing.
pub fn feedback_alert(prev: Option<&Value>, next: Option<&Value>) -> Option<Alert> {
    let f = &ds4().feedback;
    f.order.iter().find_map(|class| {
        let (rules, alert) = match class.as_str() {
            "critical" => (&f.critical, Alert::Critical),
            "warning" => (&f.warning, Alert::Warning),
            "mode_change" => (&f.mode_change, Alert::ModeChange),
            _ => return None,
        };
        rules.iter().any(|r| fires(r, prev, next)).then_some(alert)
    })
}

/// The value at a dotted path; a missing step reads as null.
fn at<'a>(s: &'a Value, path: &str) -> &'a Value {
    path.split('.').fold(s, |v, k| &v[k])
}

fn fires(rule: &FeedbackRule, prev: Option<&Value>, next: Option<&Value>) -> bool {
    if rule.on == Trigger::StateLost {
        return prev.is_some() && next.is_none();
    }
    let (Some(p), Some(n)) = (prev, next) else {
        return false;
    };
    if rule.armed_only && n["armed"] == false {
        return false;
    }
    let field = rule.field.as_deref().unwrap_or_default();
    let (a, b) = (at(p, field), at(n, field));
    match rule.on {
        Trigger::Changed => a != b,
        Trigger::NewValue => !b.is_null() && a != b,
        Trigger::Entered => a != b && rule.values.contains(b),
        Trigger::Appeared => a.is_null() && !b.is_null(),
        Trigger::NewItem => {
            let before = matching_keys(rule, a);
            matching_keys(rule, b).iter().any(|k| !before.contains(k))
        }
        Trigger::StateLost => false,
    }
}

/// The `key` values of the items of an array that match every `where` pair.
fn matching_keys<'a>(rule: &FeedbackRule, items: &'a Value) -> Vec<&'a Value> {
    let key = rule.key.as_deref().unwrap_or("id");
    items
        .as_array()
        .into_iter()
        .flatten()
        .filter(|it| rule.where_.iter().all(|(k, v)| &it[k] == v))
        .map(|it| &it[key])
        .collect()
}

/// One motor pulse.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pulse {
    pub at_ms: u64,
    pub strong: f32,
    pub weak: f32,
    pub length_ms: u16,
}

/// Queue of pulses due to play.
#[derive(Debug, Default)]
pub struct RumbleQueue {
    pending: Vec<Pulse>,
}

impl RumbleQueue {
    fn pattern(&mut self, p: &super::spec::Pulses, now_ms: u64) {
        let mut at = now_ms;
        for &len in &p.pulses_ms {
            self.pending.push(Pulse {
                at_ms: at,
                strong: p.strong,
                weak: p.weak,
                length_ms: len,
            });
            at += u64::from(len) + u64::from(p.gap_ms);
        }
    }

    pub fn alert(&mut self, a: Alert, now_ms: u64) {
        let r = &ds4().rumble;
        match a {
            Alert::ModeChange => self.pattern(&r.mode_change, now_ms),
            Alert::Warning => self.pattern(&r.warning, now_ms),
            Alert::Critical => self.pattern(&r.critical, now_ms),
        }
    }

    pub fn cue(&mut self, c: Cue, now_ms: u64) {
        let r = &ds4().rumble;
        match c {
            Cue::ModeChange => self.alert(Alert::ModeChange, now_ms),
            Cue::Countdown => self.pending.push(Pulse {
                at_ms: now_ms,
                strong: 0.6,
                weak: 0.6,
                length_ms: r.countdown.pulse_ms,
            }),
            Cue::Ramp(m) => self.pending.push(Pulse {
                at_ms: now_ms,
                strong: m,
                weak: m,
                length_ms: r.ramp.tick_ms as u16,
            }),
        }
    }

    /// The pulses due at `now_ms`, removed from the queue.
    pub fn due(&mut self, now_ms: u64) -> Vec<Pulse> {
        let (due, rest): (Vec<_>, Vec<_>) = self.pending.drain(..).partition(|p| p.at_ms <= now_ms);
        self.pending = rest;
        due
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn view(s: Value) -> PilotView {
        PilotView {
            state: Some((s, 1000)),
            relay_up: true,
        }
    }

    #[test]
    fn lightbar_state_per_pilot_state() {
        let cases = [
            (json!({"owner":"idle","armed":false}), "disarmed"),
            (json!({"owner":"idle","armed":true}), "armed_idle"),
            (json!({"owner":"pilot","armed":true}), "pilot"),
            (json!({"owner":"skill","armed":true}), "skill"),
            (json!({"owner":"mission_fc","armed":true}), "mission"),
            (
                json!({"owner":"fc_mode","fc_mode":6,"vehicle_class":"multirotor","armed":true}),
                "rth",
            ),
            (
                json!({"owner":"fc_mode","fc_mode":11,"vehicle_class":"fixedwing","armed":true}),
                "rth",
            ),
            (
                json!({"owner":"fc_mode","fc_mode":5,"vehicle_class":"multirotor","armed":true}),
                "armed_idle",
            ),
            (
                json!({"owner":"pilot","armed":true,"rth_countdown_s":7}),
                "low_battery",
            ),
            (
                json!({"owner":"pilot","armed":true,"link_loss_in_s":4}),
                "failsafe",
            ),
        ];
        for (s, want) in cases {
            assert_eq!(view(s.clone()).lightbar_state(1500), want, "{s}");
        }
        // Stale state or a down relay reads as a lost link.
        assert_eq!(
            view(json!({"owner":"pilot"})).lightbar_state(1000 + STATE_STALE_MS + 1),
            "failsafe"
        );
        assert_eq!(PilotView::default().lightbar_state(0), "failsafe");
    }

    #[test]
    fn unknown_arm_state_reads_as_possibly_airborne() {
        // The pilot sends null while it cannot hear the FC.
        let unknown = view(json!({"owner":"idle","armed":null,"landed":null}));
        assert_eq!(unknown.lightbar_state(1500), "armed_idle");
        assert!(!unknown.known_landed(1500));
        assert!(view(json!({"owner":"idle","armed":false,"landed":true})).known_landed(1500));
        assert!(!view(json!({"owner":"idle","armed":true,"landed":false})).known_landed(1500));
        // No live state at all is not known landed either.
        assert!(!PilotView::default().known_landed(0));
        assert!(!view(json!({"landed":true})).known_landed(1000 + STATE_STALE_MS));
    }

    #[test]
    fn lightbar_colours_and_patterns_follow_the_spec() {
        assert_eq!(lightbar_rgb("disarmed", 0), [0x00, 0xC8, 0x53]);
        assert_eq!(lightbar_rgb("pilot", 0), [0x29, 0x79, 0xFF]);
        assert_eq!(lightbar_rgb("skill", 0), [0x22, 0xD3, 0xEE]);
        assert_eq!(lightbar_rgb("mission", 0), [0xFF, 0x3E, 0xC8]);
        // RTH blinks at 1 Hz: on in the first half second, off in the second.
        assert_ne!(lightbar_rgb("rth", 100), [0, 0, 0]);
        assert_eq!(lightbar_rgb("rth", 600), [0, 0, 0]);
        // Failsafe blinks at 4 Hz.
        assert_ne!(lightbar_rgb("failsafe", 50), [0, 0, 0]);
        assert_eq!(lightbar_rgb("failsafe", 200), [0, 0, 0]);
    }

    #[test]
    fn transitions_rumble_by_class() {
        let a = json!({"owner":"pilot","skill":null,"hold_reason":null});
        let b = json!({"owner":"skill","skill":{"skill_id":"follow"},"hold_reason":null});
        assert_eq!(feedback_alert(Some(&a), Some(&b)), Some(Alert::ModeChange));
        let c = json!({"owner":"pilot","skill":null,"hold_reason":"obstacle-close"});
        assert_eq!(feedback_alert(Some(&a), Some(&c)), Some(Alert::Warning));
        assert_eq!(feedback_alert(Some(&a), None), Some(Alert::Critical));
        assert_eq!(feedback_alert(Some(&a), Some(&a)), None);
    }

    #[test]
    fn critical_is_three_long_pulses() {
        let mut q = RumbleQueue::default();
        q.alert(Alert::Critical, 0);
        let all: Vec<Pulse> = (0..2000).step_by(10).flat_map(|t| q.due(t)).collect();
        assert_eq!(all.len(), 3);
        assert!(all.iter().all(|p| p.length_ms == 300));
    }

    /// The alert one pilot-state update earns, through the public API.
    fn alert(prev: Option<&Value>, next: Option<&Value>) -> Option<Alert> {
        feedback_alert(prev, next)
    }

    fn flying(extra: Value) -> Value {
        let mut s = json!({
            "owner": "pilot", "profile": "normal", "armed": true, "skill": null,
            "control_mode": "ados", "hold_reason": null, "avoid_state": "clear",
            "link_loss_in_s": null, "rth_countdown_s": null, "readiness": [],
            "input_session": {"src": "gs", "session": 7, "label": "Ground station"},
        });
        for (k, v) in extra.as_object().unwrap() {
            s[k] = v.clone();
        }
        s
    }

    #[test]
    fn the_light_bar_shows_the_profile_while_the_sticks_fly() {
        for (profile, want) in [
            ("cine", "pilot_cine"),
            ("normal", "pilot_normal"),
            ("sport", "pilot_sport"),
        ] {
            assert_eq!(
                view(flying(json!({"profile": profile}))).lightbar_state(1500),
                want
            );
        }
        // Other motion owners keep the plain pilot colour; state colours keep priority.
        assert_eq!(
            view(flying(json!({"owner": "goto"}))).lightbar_state(1500),
            "pilot"
        );
        assert_eq!(
            view(flying(json!({"profile": "sport", "rth_countdown_s": 5}))).lightbar_state(1500),
            "low_battery"
        );
        assert_eq!(lightbar_rgb("pilot_cine", 0), [0x64, 0xFF, 0xDA]);
        assert_eq!(lightbar_rgb("pilot_sport", 0), [0xFF, 0x6D, 0x00]);
        assert_eq!(lightbar_rgb("pilot_normal", 0), lightbar_rgb("pilot", 0));
    }

    #[test]
    fn a_profile_change_or_a_takeover_is_a_mode_change() {
        let a = flying(json!({}));
        let sport = flying(json!({"profile": "sport"}));
        assert_eq!(alert(Some(&a), Some(&sport)), Some(Alert::ModeChange));
        let other = flying(json!({"input_session": {"src": "mc", "session": 9, "label": "MC"}}));
        assert_eq!(alert(Some(&a), Some(&other)), Some(Alert::ModeChange));
    }

    #[test]
    fn avoiding_a_speed_cap_and_a_hold_reason_are_warnings() {
        let a = flying(json!({}));
        let detour = flying(json!({"avoid_state": "detour"}));
        assert_eq!(alert(Some(&a), Some(&detour)), Some(Alert::Warning));
        let blocked = flying(json!({"avoid_state": "blocked"}));
        assert_eq!(alert(Some(&detour), Some(&blocked)), Some(Alert::Warning));
        assert_eq!(alert(Some(&blocked), Some(&blocked)), None);
        let capped = flying(json!({"speed_cap": {"reason": "gnss-denied", "v_h_mps": 3.0}}));
        assert_eq!(alert(Some(&a), Some(&capped)), Some(Alert::Warning));
        // The same cap staying on is no news.
        assert_eq!(alert(Some(&capped), Some(&capped)), None);
        let held = flying(json!({"hold_reason": "obstacle-close"}));
        assert_eq!(alert(Some(&a), Some(&held)), Some(Alert::Warning));
        let warn = flying(
            json!({"readiness": [{"id": "gps", "ok": false, "severity": "warn", "msg": ""}]}),
        );
        assert_eq!(alert(Some(&a), Some(&warn)), Some(Alert::Warning));
    }

    #[test]
    fn a_lost_controller_a_link_loss_or_a_failsafe_is_critical() {
        let a = flying(json!({}));
        let lost = flying(json!({"hold_reason": "controller-lost"}));
        assert_eq!(alert(Some(&a), Some(&lost)), Some(Alert::Critical));
        let link = flying(json!({"link_loss_in_s": 9.0}));
        assert_eq!(alert(Some(&a), Some(&link)), Some(Alert::Critical));
        // The countdown ticking down is the same loss.
        let link_later = flying(json!({"link_loss_in_s": 8.0}));
        assert_eq!(alert(Some(&link), Some(&link_later)), None);
        let fs = flying(json!({"fc_failsafe": true, "owner": "fc_mode", "fc_mode": 6}));
        assert_eq!(alert(Some(&a), Some(&fs)), Some(Alert::Critical));
        let block = flying(
            json!({"readiness": [{"id": "ekf", "ok": false, "severity": "block", "msg": ""}]}),
        );
        assert_eq!(alert(Some(&a), Some(&block)), Some(Alert::Critical));
        // A failing block item on the ground is only a pre-flight item.
        let a_ground = flying(json!({"armed": false}));
        let block_ground = flying(
            json!({"armed": false, "readiness": [{"id": "ekf", "ok": false, "severity": "block", "msg": ""}]}),
        );
        assert_eq!(alert(Some(&a_ground), Some(&block_ground)), None);
        // The pilot state going stale is critical; the first state after it earns nothing.
        assert_eq!(alert(Some(&a), None), Some(Alert::Critical));
        assert_eq!(alert(None, Some(&a)), None);
        // The strongest class wins when several fire at once.
        let both = flying(json!({"profile": "sport", "hold_reason": "controller-lost"}));
        assert_eq!(alert(Some(&a), Some(&both)), Some(Alert::Critical));
    }
}
