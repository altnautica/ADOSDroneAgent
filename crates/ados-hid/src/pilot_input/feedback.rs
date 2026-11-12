//! Controller feedback from the drone's `pilot` state: the lightbar state and
//! colour (spec priority order), the rumble cue on state transitions, and the
//! rumble pulse scheduler.

use serde_json::Value;

use super::gestures::Cue;
use super::spec::ds4;

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
            "pilot" | "goto" | "swarm" | "tuning" => "pilot",
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

/// Compare two consecutive views: a lightbar escalation into failsafe or low
/// battery is critical; a new hold reason or a blocked plan is a warning; an
/// owner or skill change is a mode change.
pub fn transition_alert(
    prev: Option<&Value>,
    next: Option<&Value>,
    prev_bar: &str,
    next_bar: &str,
) -> Option<Alert> {
    if prev_bar != next_bar && matches!(next_bar, "failsafe" | "low_battery") {
        return Some(Alert::Critical);
    }
    let (p, n) = (prev?, next?);
    if n["hold_reason"] != p["hold_reason"] && !n["hold_reason"].is_null()
        || n["avoid_state"] != p["avoid_state"] && n["avoid_state"] == "blocked"
    {
        return Some(Alert::Warning);
    }
    if n["owner"] != p["owner"] || n["skill"] != p["skill"] {
        return Some(Alert::ModeChange);
    }
    None
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
        assert_eq!(
            transition_alert(Some(&a), Some(&b), "pilot", "skill"),
            Some(Alert::ModeChange)
        );
        let c = json!({"owner":"pilot","skill":null,"hold_reason":"obstacle-close"});
        assert_eq!(
            transition_alert(Some(&a), Some(&c), "pilot", "pilot"),
            Some(Alert::Warning)
        );
        assert_eq!(
            transition_alert(Some(&a), None, "pilot", "failsafe"),
            Some(Alert::Critical)
        );
        assert_eq!(transition_alert(Some(&a), Some(&a), "pilot", "pilot"), None);
    }

    #[test]
    fn critical_is_three_long_pulses() {
        let mut q = RumbleQueue::default();
        q.alert(Alert::Critical, 0);
        let all: Vec<Pulse> = (0..2000).step_by(10).flat_map(|t| q.due(t)).collect();
        assert_eq!(all.len(), 3);
        assert!(all.iter().all(|p| p.length_ms == 300));
    }
}
