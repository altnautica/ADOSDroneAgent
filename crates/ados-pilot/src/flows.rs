//! Operator flows (C10): the single stick session, the centred-sticks
//! handover gate, hold detectors for takeover and mission pre-emption, and
//! banners.

use ados_protocol::pilot::{Banner, Gate, InputSession};
use serde_json::{Map, Value};

/// Axes below this count as centred for the gate.
pub const GATE_CENTRED: f32 = 0.1;
/// Centred this long to pass the gate.
pub const GATE_HOLD_MS: u64 = 500;
/// Banners stay up this long.
pub const BANNER_MS: u64 = 3000;

/// A surface's display name from its `src`.
pub fn src_label(src: &str) -> String {
    match src {
        "mc" => "Mission Control",
        "android" => "Android GCS",
        "cockpit" => "Cockpit",
        "kiosk" => "Ground station",
        "gs-native" => "Ground station controller",
        "bench" => "Bench",
        "api" => "API",
        other => other,
    }
    .to_owned()
}

/// The one accepted stick source.
#[derive(Debug, Clone, Default)]
pub struct Sessions {
    pub owner: Option<InputSession>,
    pub axes: [f32; 4],
    pub buttons: u32,
    gate_pending: bool,
    centred_since: Option<u64>,
    /// The session that last released, and when: a surface that briefly
    /// stops streaming (a pad held perfectly still) and resumes within
    /// [`RESUME_MS`] keeps the sticks without passing the gate again.
    released: Option<(String, u32, u64)>,
}

/// See [`Sessions::released`].
pub const RESUME_MS: u64 = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accept {
    Accepted,
    /// Another surface holds the sticks.
    OtherSession,
}

impl Sessions {
    /// Offer one input frame. The first live session owns the sticks.
    pub fn offer(
        &mut self,
        src: &str,
        session: u32,
        axes: [f32; 4],
        buttons: u32,
        now_ms: u64,
    ) -> Accept {
        match &self.owner {
            Some(o) if o.src != src || o.session != session => return Accept::OtherSession,
            Some(_) => {}
            None => {
                self.owner = Some(InputSession {
                    src: src.to_owned(),
                    session,
                    label: src_label(src),
                });
                let resumed = self.released.as_ref().is_some_and(|(s, id, at)| {
                    s == src && *id == session && now_ms.saturating_sub(*at) <= RESUME_MS
                });
                if !resumed {
                    self.arm_gate();
                }
            }
        }
        self.axes = axes.map(|a| {
            if a.is_finite() {
                a.clamp(-1.0, 1.0)
            } else {
                0.0
            }
        });
        self.buttons = buttons;
        self.update_gate(now_ms);
        Accept::Accepted
    }

    /// Hand the sticks to another surface (`session.take`).
    pub fn take(&mut self, src: &str, session: u32) {
        self.owner = Some(InputSession {
            src: src.to_owned(),
            session,
            label: src_label(src),
        });
        self.axes = [0.0; 4];
        self.arm_gate();
    }

    /// `release`: true when it ended the owning session.
    pub fn release(&mut self, src: &str, session: u32, now_ms: u64) -> bool {
        if self
            .owner
            .as_ref()
            .is_some_and(|o| o.src == src && o.session == session)
        {
            self.released = (!self.gate_pending).then(|| (src.to_owned(), session, now_ms));
            self.owner = None;
            self.axes = [0.0; 4];
            self.gate_pending = false;
            true
        } else {
            false
        }
    }

    /// Require centred sticks before they take effect (a human-initiated
    /// owner change).
    pub fn arm_gate(&mut self) {
        self.gate_pending = true;
        self.centred_since = None;
    }

    fn update_gate(&mut self, now_ms: u64) {
        if !self.gate_pending {
            return;
        }
        if self.axes.iter().all(|a| a.abs() < GATE_CENTRED) {
            let since = *self.centred_since.get_or_insert(now_ms);
            if now_ms.saturating_sub(since) >= GATE_HOLD_MS {
                self.gate_pending = false;
            }
        } else {
            self.centred_since = None;
        }
    }

    /// The axes the pilot may fly: zero while the gate is pending.
    pub fn effective_axes(&self) -> [f32; 4] {
        if self.gate_pending || self.owner.is_none() {
            [0.0; 4]
        } else {
            self.axes
        }
    }

    pub fn gate(&self, now_ms: u64) -> Option<Gate> {
        if !self.gate_pending || self.owner.is_none() {
            return None;
        }
        let progress = self.centred_since.map_or(0.0, |s| {
            (now_ms.saturating_sub(s) as f64 / GATE_HOLD_MS as f64).min(1.0)
        });
        Some(Gate {
            pending: true,
            progress,
        })
    }

    pub fn gate_pending(&self) -> bool {
        self.gate_pending && self.owner.is_some()
    }
}

/// Fires once a condition has held for `hold_ms`.
#[derive(Debug, Clone, Default)]
pub struct HoldDetector {
    since: Option<u64>,
    fired: bool,
}

impl HoldDetector {
    pub fn update(&mut self, active: bool, now_ms: u64, hold_ms: u64) -> bool {
        if !active {
            self.since = None;
            self.fired = false;
            return false;
        }
        let since = *self.since.get_or_insert(now_ms);
        if !self.fired && now_ms.saturating_sub(since) >= hold_ms {
            self.fired = true;
            return true;
        }
        false
    }
}

/// Build a banner.
pub fn banner(text_key: &str, args: &[(&str, Value)], now_ms: u64) -> Banner {
    let mut m = Map::new();
    for (k, v) in args {
        m.insert((*k).to_owned(), v.clone());
    }
    Banner {
        text_key: text_key.to_owned(),
        args: m,
        until_ms: now_ms + BANNER_MS,
    }
}

/// Steps of the `launch` sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchStep {
    Checks,
    Arm,
    Takeoff,
    After,
    Done,
}

impl LaunchStep {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Checks => "checks",
            Self::Arm => "arm",
            Self::Takeoff => "takeoff",
            Self::After => "after",
            Self::Done => "done",
        }
    }
}

/// A running `launch` sequence.
#[derive(Debug, Clone, PartialEq)]
pub struct Launch {
    pub step: LaunchStep,
    /// `hold`, `skill`, `mission` or `goto`.
    pub after: String,
    pub skill: Option<(String, String)>,
    pub goto: Option<[f32; 3]>,
    pub alt_m: f32,
    pub when_done: Option<String>,
    pub error: Option<String>,
    pub step_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_session_is_ignored_until_take() {
        let mut s = Sessions::default();
        assert_eq!(s.offer("mc", 1, [0.0; 4], 0, 0), Accept::Accepted);
        assert_eq!(s.offer("android", 9, [0.5; 4], 0, 10), Accept::OtherSession);
        s.take("android", 9);
        assert_eq!(s.owner.as_ref().unwrap().src, "android");
        assert_eq!(s.offer("mc", 1, [0.0; 4], 0, 20), Accept::OtherSession);
    }

    #[test]
    fn brief_release_resumes_without_the_gate() {
        let mut s = Sessions::default();
        s.offer("mc", 1, [0.0; 4], 0, 0);
        s.offer("mc", 1, [0.0; 4], 0, 600);
        assert!(!s.gate_pending());
        assert!(s.release("mc", 1, 700));
        s.offer("mc", 1, [0.0, 1.0, 0.0, 0.0], 0, 900);
        assert_eq!(s.effective_axes()[1], 1.0);
        assert!(s.release("mc", 1, 1000));
        s.offer("mc", 1, [0.0, 1.0, 0.0, 0.0], 0, 3000);
        assert!(s.gate_pending(), "a long gap passes the gate again");
    }

    #[test]
    fn gate_needs_half_a_second_centred() {
        let mut s = Sessions::default();
        s.offer("mc", 1, [0.0, 0.5, 0.0, 0.0], 0, 0);
        assert!(s.gate_pending());
        assert_eq!(s.effective_axes(), [0.0; 4]);
        s.offer("mc", 1, [0.0, 0.5, 0.0, 0.0], 0, 1000);
        assert!(s.gate_pending(), "deflected sticks never pass the gate");
        s.offer("mc", 1, [0.0; 4], 0, 1100);
        s.offer("mc", 1, [0.05, 0.0, 0.0, 0.0], 0, 1400);
        assert!((s.gate(1400).unwrap().progress - 0.6).abs() < 1e-3);
        s.offer("mc", 1, [0.0; 4], 0, 1600);
        assert!(!s.gate_pending());
        s.offer("mc", 1, [0.0, 0.5, 0.0, 0.0], 0, 1620);
        assert_eq!(s.effective_axes()[1], 0.5);
    }

    #[test]
    fn hold_detector_fires_once() {
        let mut h = HoldDetector::default();
        assert!(!h.update(true, 0, 1000));
        assert!(!h.update(true, 999, 1000));
        assert!(h.update(true, 1000, 1000));
        assert!(!h.update(true, 2000, 1000));
        assert!(!h.update(false, 2100, 1000));
        assert!(!h.update(true, 2200, 1000));
    }
}
