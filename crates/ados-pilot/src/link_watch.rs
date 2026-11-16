//! The link-loss watch over each operator instance, used with no RC
//! transmitter (`pilot.rc_receiver: absent`).
//!
//! With an RC transmitter the pilot keeps one shared timer: any surface frame
//! refreshes it ([`crate::failsafe::Failsafe`]). With none, a second surface
//! must never stand in for a lost one: an unattended Mission Control tab that
//! sends `hb` on a timer would hide the phone's loss, and two phones share one
//! `src`. So every surface frame carries its instance id (`sid`, one per app
//! process or browser tab, kept across reconnects), and each instance has its
//! own timer and its own fired state. Frames without an `sid` (older surfaces)
//! share one legacy timer, armed only once such a frame arrives.
//!
//! An instance leaves the watch only through an honoured `bye`: while
//! disarmed, or while another instance holds the stick session (a positive
//! takeover). Any other `bye` is silence, and its timer runs on.

use std::collections::BTreeMap;

use ados_protocol::pilot::InputSession;

/// The `src` reported for the legacy timer's surfaces.
pub const LEGACY: &str = "surface";

/// An instance heard within this before arming is live and watched in the
/// flight: three of the surfaces' 1 s `hb`.
pub const ARM_LIVE_MS: u64 = 3000;

#[derive(Debug, Clone)]
struct Instance {
    src: String,
    last_ms: u64,
    fired: bool,
    /// The stick session it last streamed or took, if any.
    session: Option<u32>,
}

#[derive(Debug, Clone, Default)]
pub struct Instances {
    map: BTreeMap<String, Instance>,
    legacy_last_ms: Option<u64>,
    legacy_fired: bool,
}

impl Instances {
    /// A frame from `src`, instance `sid` (None: a surface without one).
    pub fn heard(&mut self, sid: Option<&str>, src: &str, now_ms: u64) {
        match sid {
            Some(s) => {
                let i = self.map.entry(s.to_owned()).or_insert_with(|| Instance {
                    src: src.to_owned(),
                    last_ms: now_ms,
                    fired: false,
                    session: None,
                });
                i.src = src.to_owned();
                i.last_ms = now_ms;
                i.fired = false;
            }
            None => {
                self.legacy_last_ms = Some(now_ms);
                self.legacy_fired = false;
            }
        }
    }

    /// The stick session instance `sid` streams or took.
    pub fn session(&mut self, sid: Option<&str>, session: u32) {
        if let Some(i) = sid.and_then(|s| self.map.get_mut(s)) {
            i.session = Some(session);
        }
    }

    /// Whether a `bye` from `sid` ends its watch: always while disarmed;
    /// armed, only when another instance holds the stick session `owner`.
    pub fn bye_honoured(&self, sid: &str, armed: bool, owner: Option<&InputSession>) -> bool {
        if !armed {
            return true;
        }
        let Some(o) = owner else {
            return false;
        };
        self.map
            .iter()
            .any(|(k, i)| k != sid && i.src == o.src && i.session == Some(o.session))
    }

    /// End the watch of `sid` (an honoured `bye`).
    pub fn leave(&mut self, sid: &str) {
        self.map.remove(sid);
    }

    /// The `src` of the first watched instance silent for `window_ms` that
    /// has not fired yet; it is marked fired. One call fires one instance.
    pub fn lost(&mut self, now_ms: u64, window_ms: u64) -> Option<String> {
        for i in self.map.values_mut() {
            if !i.fired && now_ms.saturating_sub(i.last_ms) >= window_ms {
                i.fired = true;
                return Some(i.src.clone());
            }
        }
        if !self.legacy_fired
            && self
                .legacy_last_ms
                .is_some_and(|t| now_ms.saturating_sub(t) >= window_ms)
        {
            self.legacy_fired = true;
            return Some(LEGACY.to_owned());
        }
        None
    }

    /// The `src` of an instance whose link loss fired and that has not
    /// spoken since (nor left through an honoured `bye`).
    pub fn unrecovered(&self) -> Option<&str> {
        self.map
            .values()
            .find(|i| i.fired)
            .map(|i| i.src.as_str())
            .or(self.legacy_fired.then_some(LEGACY))
    }

    /// The longest silence among watched instances that have not fired, for
    /// the link-loss countdown.
    pub fn worst_silence_ms(&self, now_ms: u64) -> Option<u64> {
        let inst = self
            .map
            .values()
            .filter(|i| !i.fired)
            .map(|i| now_ms.saturating_sub(i.last_ms));
        let legacy = self
            .legacy_last_ms
            .filter(|_| !self.legacy_fired)
            .map(|t| now_ms.saturating_sub(t));
        inst.chain(legacy).max()
    }

    /// Forget every instance (on disarm): the next flight watches only those
    /// that speak after it.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// On arming: the flight watches the instances that are live now. One
    /// silent for `live_ms` or more (an app the OS killed, a reloaded tab
    /// that came back under a new `sid`, a closed dashboard) is forgotten,
    /// and so is a legacy timer as quiet: left in, it would fire the
    /// link-loss action right after take-off. A live surface sends `hb`
    /// every second, so it stays.
    pub fn armed(&mut self, now_ms: u64, live_ms: u64) {
        self.map
            .retain(|_, i| now_ms.saturating_sub(i.last_ms) < live_ms);
        if self
            .legacy_last_ms
            .is_some_and(|t| now_ms.saturating_sub(t) >= live_ms)
        {
            self.legacy_last_ms = None;
            self.legacy_fired = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(src: &str, session: u32) -> InputSession {
        InputSession {
            src: src.into(),
            session,
            label: String::new(),
        }
    }

    #[test]
    fn each_instance_fires_on_its_own() {
        let mut w = Instances::default();
        w.heard(Some("phone"), "android", 0);
        w.heard(Some("tab"), "mc", 0);
        for t in (1000..=20_000).step_by(1000) {
            w.heard(Some("tab"), "mc", t);
        }
        assert_eq!(w.lost(9_999, 10_000), None);
        assert_eq!(w.lost(10_000, 10_000).as_deref(), Some("android"));
        assert_eq!(w.lost(10_500, 10_000), None, "fires once");
        assert_eq!(w.unrecovered(), Some("android"));
        w.heard(Some("phone"), "android", 11_000);
        assert_eq!(w.unrecovered(), None);
    }

    #[test]
    fn two_phones_with_one_src_are_two_instances() {
        let mut w = Instances::default();
        w.heard(Some("a"), "android", 0);
        w.heard(Some("b"), "android", 0);
        w.heard(Some("b"), "android", 9_000);
        assert_eq!(w.lost(10_000, 10_000).as_deref(), Some("android"));
        assert_eq!(w.lost(10_000, 10_000), None);
    }

    #[test]
    fn a_fired_tab_never_masks_a_later_phone_silence() {
        let mut w = Instances::default();
        w.heard(Some("tab"), "mc", 0);
        w.heard(Some("phone"), "android", 5_000);
        assert_eq!(w.lost(10_000, 10_000).as_deref(), Some("mc"));
        w.heard(Some("phone"), "android", 12_000);
        assert_eq!(
            w.unrecovered(),
            Some("mc"),
            "the phone does not clear the tab"
        );
        assert_eq!(w.lost(22_000, 10_000).as_deref(), Some("android"));
    }

    #[test]
    fn a_reconnect_keeps_the_instance() {
        let mut w = Instances::default();
        w.heard(Some("phone"), "android", 0);
        w.heard(Some("phone"), "android", 8_000);
        assert_eq!(w.lost(12_000, 10_000), None);
    }

    #[test]
    fn bye_ends_the_watch_only_disarmed_or_after_a_takeover() {
        let mut w = Instances::default();
        w.heard(Some("phone"), "android", 0);
        w.heard(Some("tab"), "mc", 0);
        assert!(w.bye_honoured("phone", false, None));
        assert!(
            !w.bye_honoured("phone", true, None),
            "armed, nobody took over"
        );
        let tab_holds = owner("mc", 4);
        assert!(
            !w.bye_honoured("phone", true, Some(&tab_holds)),
            "the tab only sends hb"
        );
        w.session(Some("tab"), 4);
        assert!(w.bye_honoured("phone", true, Some(&tab_holds)));
        w.session(Some("phone"), 9);
        assert!(
            !w.bye_honoured("phone", true, Some(&owner("android", 9))),
            "its own session is no takeover"
        );
    }

    #[test]
    fn the_legacy_timer_runs_only_after_a_frame_without_an_id() {
        let mut w = Instances::default();
        w.heard(Some("phone"), "android", 0);
        w.heard(Some("phone"), "android", 30_000);
        assert_eq!(w.lost(30_000, 10_000), None);
        w.heard(None, "cockpit", 31_000);
        assert_eq!(w.lost(41_000, 10_000).as_deref(), Some("android"));
        assert_eq!(w.lost(41_000, 10_000).as_deref(), Some(LEGACY));
    }

    #[test]
    fn the_countdown_follows_the_longest_silence() {
        let mut w = Instances::default();
        assert_eq!(w.worst_silence_ms(5_000), None);
        w.heard(Some("phone"), "android", 1_000);
        w.heard(Some("tab"), "mc", 4_000);
        assert_eq!(w.worst_silence_ms(6_000), Some(5_000));
        w.clear();
        assert_eq!(w.worst_silence_ms(6_000), None);
    }
}
