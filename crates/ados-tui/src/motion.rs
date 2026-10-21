//! Motion: the effect level and the transitions drawn over the buffer.
//!
//! Every effect is a [`tachyonfx`] effect applied to the rendered buffer after
//! the widgets are drawn. Each trigger has one method on [`Fx`]; a method is a
//! no-op at a level where the trigger has no motion, so callers never branch on
//! the level themselves.

use std::time::Instant;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use tachyonfx::{fx, EffectManager, Interpolation, Motion};

use crate::theme::Tier;

/// How much motion the UI draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotionLevel {
    Full,
    Reduced,
    Off,
}

/// Resolve the effect level from the preference (`auto`/`full`/`reduced`/
/// `off`), whether the session runs over SSH, and the colour tier. `auto` is
/// reduced over SSH and full locally; a basic or plain colour tier forces off,
/// since colour interpolation needs real colours.
pub fn resolve(pref: &str, ssh: bool, tier: Tier) -> MotionLevel {
    if matches!(tier, Tier::Basic | Tier::None) {
        return MotionLevel::Off;
    }
    match pref {
        "full" => MotionLevel::Full,
        "reduced" => MotionLevel::Reduced,
        "off" => MotionLevel::Off,
        _ if ssh => MotionLevel::Reduced,
        _ => MotionLevel::Full,
    }
}

/// Whether the process runs inside an SSH session.
pub fn env_ssh() -> bool {
    std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some()
}

/// The running effects and the level they are drawn at.
pub struct Fx {
    mgr: EffectManager<&'static str>,
    pub level: MotionLevel,
    last: Instant,
    intro_done: bool,
    /// Running pulses and the rect each was started on.
    pulses: Vec<(&'static str, Rect)>,
}

impl Fx {
    pub fn new(level: MotionLevel) -> Self {
        Self {
            mgr: EffectManager::default(),
            level,
            last: Instant::now(),
            intro_done: false,
            pulses: Vec::new(),
        }
    }

    /// Change the level. Effects already running finish; new ones follow the
    /// new level. Turning motion off cancels everything at once.
    pub fn set_level(&mut self, level: MotionLevel) {
        self.level = level;
        if level != MotionLevel::Full {
            for (key, _) in std::mem::take(&mut self.pulses) {
                self.mgr.cancel_unique_effect(key);
            }
        }
        if level == MotionLevel::Off {
            self.mgr = EffectManager::default();
        }
    }

    fn add(&mut self, key: &'static str, effect: tachyonfx::Effect) {
        self.mgr.add_unique_effect(key, effect);
    }

    /// Tab switch: fade the body in from the canvas.
    pub fn tab_switch(&mut self, area: Rect, canvas: Color) {
        if self.level == MotionLevel::Full {
            let e = fx::fade_from(canvas, canvas, (220, Interpolation::QuadOut)).with_area(area);
            self.add("tab", e);
        }
    }

    /// Overlay opened: fade the overlay rect in.
    pub fn overlay(&mut self, area: Rect, canvas: Color) {
        if self.level != MotionLevel::Off {
            let e = fx::fade_from(canvas, canvas, (150, Interpolation::QuadOut)).with_area(area);
            self.add("overlay", e);
        }
    }

    /// A toast appeared at `area`.
    pub fn toast_in(&mut self, area: Rect, canvas: Color) {
        let e = match self.level {
            MotionLevel::Full => fx::slide_in(
                Motion::RightToLeft,
                6,
                0,
                canvas,
                (180, Interpolation::QuadOut),
            ),
            MotionLevel::Reduced => fx::fade_from_fg(canvas, 150),
            MotionLevel::Off => return,
        };
        self.mgr.add_effect(e.with_area(area));
    }

    /// A toast is leaving `area`.
    pub fn toast_out(&mut self, area: Rect, canvas: Color) {
        let e = match self.level {
            MotionLevel::Full => fx::fade_to(canvas, canvas, 200),
            MotionLevel::Reduced => fx::fade_to_fg(canvas, 150),
            MotionLevel::Off => return,
        };
        self.mgr.add_effect(e.with_area(area));
    }

    /// A value was applied: flash the row from the accent colour.
    pub fn applied(&mut self, area: Rect, accent: Color) {
        if self.level != MotionLevel::Off {
            self.mgr
                .add_effect(fx::fade_from_fg(accent, 600).with_area(area));
        }
    }

    /// A value failed to apply: flash the row from the error colour.
    pub fn failed(&mut self, area: Rect, error: Color) {
        if self.level != MotionLevel::Off {
            self.mgr
                .add_effect(fx::fade_from_fg(error, 800).with_area(area));
        }
    }

    /// Start, keep, or stop a repeating pulse. Called every frame: a pulse
    /// already running on the same rect is left alone, so it never restarts.
    fn pulse(&mut self, key: &'static str, area: Rect, on: bool) {
        let current = self.pulses.iter().position(|(k, _)| *k == key);
        if on && self.level == MotionLevel::Full {
            if current.is_some_and(|i| self.pulses[i].1 == area) {
                return;
            }
            let e = fx::repeating(fx::ping_pong(fx::hsl_shift_fg([0.0, 0.0, -25.0], 900)))
                .with_area(area);
            self.add(key, e);
            match current {
                Some(i) => self.pulses[i].1 = area,
                None => self.pulses.push((key, area)),
            }
        } else if let Some(i) = current {
            self.pulses.remove(i);
            self.mgr.cancel_unique_effect(key);
        }
    }

    /// Pulse the ARMED chip while the vehicle is armed.
    pub fn armed_pulse(&mut self, area: Rect, on: bool) {
        self.pulse("armed", area, on);
    }

    /// Pulse the OFFLINE/STALE chip while the agent is not answering.
    pub fn offline_pulse(&mut self, area: Rect, on: bool) {
        self.pulse("offline", area, on);
    }

    /// The first data arrival: sweep the whole screen in, once per session.
    pub fn intro(&mut self, area: Rect, canvas: Color) {
        if self.intro_done {
            return;
        }
        self.intro_done = true;
        if self.level == MotionLevel::Full {
            let e = fx::sweep_in(
                Motion::LeftToRight,
                30,
                0,
                canvas,
                (450, Interpolation::QuadOut),
            )
            .with_area(area);
            self.add("intro", e);
        }
    }

    /// Advance every running effect by the time since the last call and draw
    /// it into `buf`.
    pub fn process(&mut self, buf: &mut Buffer, area: Rect) {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.last);
        self.last = now;
        if !self.mgr.is_running() {
            return;
        }
        self.mgr
            .process_effects(tachyonfx::Duration::from(elapsed), buf, area);
    }

    /// Whether any effect still needs frames.
    pub fn running(&self) -> bool {
        self.mgr.is_running()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_follows_the_session_and_plain_tiers_force_off() {
        assert_eq!(resolve("auto", true, Tier::Truecolor), MotionLevel::Reduced);
        assert_eq!(resolve("auto", false, Tier::Truecolor), MotionLevel::Full);
        assert_eq!(resolve("auto", false, Tier::Ansi256), MotionLevel::Full);
        for pref in ["auto", "full", "reduced", "off"] {
            assert_eq!(resolve(pref, false, Tier::None), MotionLevel::Off);
            assert_eq!(resolve(pref, true, Tier::Basic), MotionLevel::Off);
        }
        assert_eq!(resolve("full", true, Tier::Truecolor), MotionLevel::Full);
        assert_eq!(resolve("off", false, Tier::Truecolor), MotionLevel::Off);
    }
}
