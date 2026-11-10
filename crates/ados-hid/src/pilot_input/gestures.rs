//! The native gesture engine: tap and hold bindings (with modifier combos and
//! `when` conditions) over the per-tick pad state, emitting actions and rumble cues.

use super::pad::{bit, PadState};
use super::spec::Binding;

/// Flight context the `when` conditions read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Context {
    pub rth_active: bool,
    pub landed: bool,
    pub vtol: bool,
}

/// A rumble cue the feedback layer plays.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Cue {
    /// One mode-change pulse (an action fired).
    ModeChange,
    /// A ramp tick at this magnitude (0..1) during a hold.
    Ramp(f32),
    /// A countdown pulse during a hold.
    Countdown,
}

/// What one tick produced.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Tick {
    pub actions: Vec<String>,
    pub cues: Vec<Cue>,
}

#[derive(Debug, Clone)]
struct Row {
    binding: Binding,
    control: u32,
    with: u32,
}

#[derive(Debug, Clone, Copy, Default)]
struct HoldState {
    since_ms: Option<u64>,
    fired: bool,
    last_cue_ms: u64,
}

/// The engine over one binding set.
#[derive(Debug, Clone)]
pub struct Gestures {
    rows: Vec<Row>,
    holds: Vec<HoldState>,
    prev: u32,
    /// Controls whose press or hold row already fired while they stay down:
    /// their tap rows do not fire on release.
    episode: u32,
}

/// The most specific rows (most modifiers) among `matches`, so Options +
/// Circle does not also fire Circle's own row.
fn most_specific(rows: &[Row], matches: Vec<usize>) -> Vec<usize> {
    let best = matches.iter().map(|&i| rows[i].with.count_ones()).max();
    matches
        .into_iter()
        .filter(|&i| Some(rows[i].with.count_ones()) == best)
        .collect()
}

fn mask(controls: &[String]) -> Option<u32> {
    controls
        .iter()
        .try_fold(0u32, |m, c| bit(c).map(|b| m | (1 << b)))
}

fn when_ok(when: Option<&str>, ctx: &Context, st: &PadState) -> bool {
    match when {
        None => true,
        Some("not_rth") => !ctx.rth_active,
        Some("rth_active") => ctx.rth_active,
        Some("on_ground") => ctx.landed,
        Some("airborne") => !ctx.landed,
        Some("vehicle_vtol") => ctx.vtol,
        Some("stick_l_centered") => super::pad::is_centred(st.stick_l),
        // Browser-only conditions (picker state) never hold natively.
        Some(_) => false,
    }
}

impl Gestures {
    pub fn new(bindings: &[Binding]) -> Self {
        let rows: Vec<Row> = bindings
            .iter()
            .filter_map(|b| {
                Some(Row {
                    control: 1 << bit(&b.control)?,
                    with: mask(&b.with)?,
                    binding: b.clone(),
                })
            })
            .collect();
        let holds = vec![HoldState::default(); rows.len()];
        Self {
            rows,
            holds,
            prev: 0,
            episode: 0,
        }
    }

    /// Forget every press (pad reconnect).
    pub fn reset(&mut self) {
        self.prev = 0;
        self.episode = 0;
        self.holds
            .iter_mut()
            .for_each(|h| *h = HoldState::default());
    }

    /// Advance one tick at `now_ms`.
    pub fn tick(&mut self, st: &PadState, ctx: &Context, now_ms: u64) -> Tick {
        let mut out = Tick::default();
        let held = st.buttons;
        let rising = held & !self.prev;
        let falling = self.prev & !held;
        let fire = |out: &mut Tick, row: &Row| {
            out.actions.push(row.binding.action.clone());
            if row.binding.rumble.is_some() {
                out.cues.push(Cue::ModeChange);
            }
        };

        // Press rows fire on button-down.
        let presses: Vec<usize> = (0..self.rows.len())
            .filter(|&i| {
                let r = &self.rows[i];
                r.binding.gesture == "press"
                    && rising & r.control != 0
                    && held & r.with == r.with
                    && when_ok(r.binding.when.as_deref(), ctx, st)
            })
            .collect();
        for i in most_specific(&self.rows, presses) {
            fire(&mut out, &self.rows[i]);
            self.episode |= self.rows[i].control;
        }

        // Tap rows fire on release, unless a press or hold row on that
        // control fired while it was down; modifiers are read as held just
        // before the release.
        let prev = self.prev;
        let episode = self.episode;
        let taps: Vec<usize> = (0..self.rows.len())
            .filter(|&i| {
                let r = &self.rows[i];
                r.binding.gesture == "tap"
                    && falling & r.control != 0
                    && episode & r.control == 0
                    && prev & r.with == r.with
                    && when_ok(r.binding.when.as_deref(), ctx, st)
            })
            .collect();
        for i in most_specific(&self.rows, taps) {
            fire(&mut out, &self.rows[i]);
        }

        let cfg = super::spec::ds4();
        for (row, hold) in self.rows.iter().zip(self.holds.iter_mut()) {
            if row.binding.gesture != "hold" {
                continue;
            }
            let all = row.control | row.with;
            let active = held & all == all && when_ok(row.binding.when.as_deref(), ctx, st);
            if !active {
                *hold = HoldState::default();
                continue;
            }
            let since = *hold.since_ms.get_or_insert(now_ms);
            if hold.fired {
                continue;
            }
            let need = row.binding.hold_ms.unwrap_or(0);
            let elapsed = now_ms.saturating_sub(since);
            if elapsed >= need {
                hold.fired = true;
                self.episode |= row.control;
                out.actions.push(row.binding.action.clone());
                out.cues.push(Cue::ModeChange);
                continue;
            }
            match row.binding.rumble.as_deref() {
                Some("ramp")
                    if now_ms.saturating_sub(hold.last_cue_ms) >= cfg.rumble.ramp.tick_ms =>
                {
                    hold.last_cue_ms = now_ms;
                    let p = elapsed as f32 / need.max(1) as f32;
                    let r = &cfg.rumble.ramp;
                    out.cues.push(Cue::Ramp(r.from + (r.to - r.from) * p));
                }
                Some("countdown")
                    if elapsed > 0
                        && now_ms.saturating_sub(hold.last_cue_ms)
                            >= cfg.rumble.countdown.every_ms =>
                {
                    hold.last_cue_ms = now_ms;
                    out.cues.push(Cue::Countdown);
                }
                _ => {}
            }
        }
        self.episode &= held;
        self.prev = held;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pilot_input::bindings::ControlsDoc;

    fn st(controls: &[&str]) -> PadState {
        let mut s = PadState::default();
        for c in controls {
            s.buttons |= 1 << bit(c).unwrap();
        }
        s
    }

    fn engine() -> Gestures {
        Gestures::new(&ControlsDoc::ados_default().native_bindings())
    }

    fn row(
        control: &str,
        with: &[&str],
        gesture: &str,
        hold_ms: Option<u64>,
        when: Option<&str>,
        action: &str,
    ) -> Binding {
        Binding {
            control: control.into(),
            with: with.iter().map(|s| s.to_string()).collect(),
            gesture: gesture.into(),
            hold_ms,
            when: when.map(str::to_string),
            action: action.into(),
            rumble: Some("countdown".into()),
            locked: false,
        }
    }

    /// Press `controls` for `ms`, then release; every action fired, in order.
    fn press_for(g: &mut Gestures, ctx: &Context, controls: &[&str], ms: u64) -> Vec<String> {
        let mut fired = Vec::new();
        for t in (0..=ms).step_by(20) {
            fired.extend(g.tick(&st(controls), ctx, t).actions);
        }
        fired.extend(g.tick(&st(&[]), ctx, ms + 20).actions);
        fired
    }

    #[test]
    fn press_fires_at_down_and_the_hold_on_the_same_control_still_fires_at_2s() {
        let mut g = Gestures::new(&[
            row("ps", &[], "press", None, None, "pause"),
            row("ps", &[], "hold", Some(2000), None, "rth"),
        ]);
        let ctx = Context::default();
        assert_eq!(g.tick(&st(&["ps"]), &ctx, 0).actions, vec!["pause"]);
        let mut fired = Vec::new();
        let mut countdowns = 0;
        for ms in (20..=2100).step_by(20) {
            let t = g.tick(&st(&["ps"]), &ctx, ms);
            countdowns += t.cues.iter().filter(|c| **c == Cue::Countdown).count();
            fired.extend(t.actions);
        }
        assert_eq!(fired, vec!["rth"], "no release needed");
        assert!(countdowns >= 3, "rumble countdown during the hold");
    }

    #[test]
    fn tap_fires_on_release_only_when_no_press_or_hold_fired() {
        let rows = [
            row("triangle", &[], "tap", None, None, "view_next"),
            row("triangle", &[], "hold", Some(1500), None, "land"),
        ];
        let ctx = Context::default();
        let mut g = Gestures::new(&rows);
        assert!(
            g.tick(&st(&["triangle"]), &ctx, 0).actions.is_empty(),
            "nothing at down"
        );
        assert_eq!(g.tick(&st(&[]), &ctx, 100).actions, vec!["view_next"]);
        let mut g = Gestures::new(&rows);
        assert_eq!(press_for(&mut g, &ctx, &["triangle"], 1600), vec!["land"]);
    }

    #[test]
    fn the_spec_ps_pauses_once_on_a_short_press() {
        let mut g = engine();
        assert_eq!(
            press_for(&mut g, &Context::default(), &["ps"], 100),
            vec!["pause"]
        );
    }

    #[test]
    fn ps_during_rth_cancels_to_hold() {
        let mut g = engine();
        let ctx = Context {
            rth_active: true,
            ..Default::default()
        };
        assert_eq!(press_for(&mut g, &ctx, &["ps"], 100), vec!["rth_cancel"]);
    }

    #[test]
    fn options_circle_is_the_ps_fallback_and_circle_alone_is_not_native() {
        let ctx = Context::default();
        let mut g = engine();
        let mut fired = g.tick(&st(&["options"]), &ctx, 0).actions;
        fired.extend(press_for(&mut g, &ctx, &["options", "circle"], 100));
        assert_eq!(fired, vec!["pause"]);
        let mut g = engine();
        assert!(press_for(&mut g, &ctx, &["circle"], 100).is_empty());
    }

    #[test]
    fn arm_needs_the_left_stick_centred_for_the_whole_hold() {
        let ctx = Context {
            landed: true,
            ..Default::default()
        };
        let mut g = engine();
        let mut fired = Vec::new();
        for ms in (0..=2100).step_by(20) {
            fired.extend(g.tick(&st(&["l1", "r1"]), &ctx, ms).actions);
        }
        assert_eq!(fired, vec!["arm"]);

        let mut g = engine();
        let mut fired = Vec::new();
        for ms in (0..=2100).step_by(20) {
            let mut s = st(&["l1", "r1"]);
            if ms == 1000 {
                s.stick_l = (0.0, -0.5);
            }
            fired.extend(g.tick(&s, &ctx, ms).actions);
        }
        assert!(
            fired.is_empty(),
            "a deflection restarts the hold: {fired:?}"
        );
    }

    #[test]
    fn kill_after_3s_of_l2_r2_circle_and_elrs_after_2s_of_l2_r2_share() {
        let ctx = Context::default();
        let mut g = engine();
        let mut fired = Vec::new();
        for ms in (0..=3000).step_by(20) {
            fired.extend(g.tick(&st(&["l2", "r2", "circle"]), &ctx, ms).actions);
        }
        assert_eq!(fired, vec!["kill"]);
        let mut g = engine();
        let mut fired = Vec::new();
        for ms in (0..=2000).step_by(20) {
            fired.extend(g.tick(&st(&["l2", "r2", "share"]), &ctx, ms).actions);
        }
        assert_eq!(fired, vec!["control_mode:elrs"]);
    }

    #[test]
    fn releasing_early_fires_nothing() {
        let ctx = Context::default();
        let mut g = engine();
        let mut fired = Vec::new();
        for ms in (0..=2900).step_by(20) {
            fired.extend(g.tick(&st(&["l2", "r2", "circle"]), &ctx, ms).actions);
        }
        fired.extend(g.tick(&st(&[]), &ctx, 2920).actions);
        assert!(fired.is_empty());
    }

    /// The native engine for a document whose kill row is `kill`.
    fn engine_with_kill(kill: Binding) -> Gestures {
        let mut doc = ControlsDoc::ados_default();
        doc.bindings.push(kill);
        Gestures::new(&doc.native_bindings())
    }

    #[test]
    fn a_kill_bound_to_press_tap_or_a_short_hold_waits_for_the_stop_hold() {
        let stop = crate::pilot_input::spec::stop_hold_ms();
        assert_eq!(stop, 3000);
        let ctx = Context::default();
        for kill in [
            row("triangle", &[], "press", None, None, "kill"),
            row("triangle", &[], "tap", None, None, "kill"),
            row("triangle", &[], "hold", Some(1500), None, "kill"),
            row("triangle", &[], "hold", Some(1), None, "kill"),
        ] {
            let shape = format!("{} {:?}", kill.gesture, kill.hold_ms);
            // A short press, released: nothing.
            let mut g = engine_with_kill(kill.clone());
            assert!(
                press_for(&mut g, &ctx, &["triangle"], 100).is_empty(),
                "{shape}: a short press"
            );
            // Held: nothing before the stop hold, the kill at it.
            let mut g = engine_with_kill(kill);
            let mut fired = Vec::new();
            for ms in (0..stop).step_by(20) {
                fired.extend(g.tick(&st(&["triangle"]), &ctx, ms).actions);
            }
            assert!(
                fired.is_empty(),
                "{shape}: fired before {stop} ms: {fired:?}"
            );
            assert_eq!(
                g.tick(&st(&["triangle"]), &ctx, stop).actions,
                vec!["kill"],
                "{shape}: fires at {stop} ms"
            );
        }
    }

    #[test]
    fn a_kill_hold_at_or_above_the_stop_hold_is_kept() {
        let doc = {
            let mut d = ControlsDoc::ados_default();
            d.bindings
                .push(row("triangle", &[], "hold", Some(4000), None, "kill"));
            d
        };
        let rows = doc.native_bindings();
        let kills: Vec<_> = rows.iter().filter(|b| b.action == "kill").collect();
        assert!(kills.iter().all(|b| b.gesture == "hold"));
        assert!(kills
            .iter()
            .any(|b| b.control == "triangle" && b.hold_ms == Some(4000)));
        assert!(kills
            .iter()
            .any(|b| b.control == "circle" && b.hold_ms == Some(3000)));
    }
}
