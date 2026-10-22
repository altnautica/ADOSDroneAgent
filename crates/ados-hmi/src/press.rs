//! Button press classification.
//!
//! Classifies each press of an active-low GPIO button as short, long or
//! cancelled on the RELEASE edge, with a recent-edge debounce guard, and
//! resolves the action bound to `<label>_<kind>` in a shared, hot-swappable
//! action mapping. A rotary encoder's detents (see [`crate::encoder`]) resolve
//! through the same mapping as `ENC_CW_short` / `ENC_CCW_short`, and its push
//! switch classifies like a button under [`ENC_SW`].
//!
//! [`PressClassifier`] is pure and host-portable: it takes synthetic
//! `(pin, edge, ts_ms)` events, so the debounce guard, the short/long
//! thresholds and the cancel-drop are unit-testable without hardware.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::encoder::{Rotation, ENC_SW};

/// Held >= this fires a "long" press (on release, never at the mark).
pub const LONG_PRESS_SECONDS: f64 = 2.0;

/// Held >= this drops the event entirely (operator changed their mind).
pub const CANCEL_HOLD_SECONDS: f64 = 6.0;

/// Software recent-edge debounce window.
pub const BOUNCE_MS: u64 = 20;

/// The friendly button id of `pin` on a board whose buttons are `pins` (role
/// order -> `B1`..`B4`), used as the mapping key prefix. Anything else
/// resolves as `BX<pin>` so extra GPIO buttons get a stable key.
pub fn pin_label(pins: &[u32; 4], pin: u32) -> String {
    match pins.iter().position(|&p| p == pin) {
        Some(slot) => format!("B{}", slot + 1),
        None => format!("BX{pin}"),
    }
}

/// Edge polarity. Buttons are active-low with an internal pull-up: a press
/// pulls the pin to ground (falling edge), a release lets it float high
/// (rising edge).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    /// Press: pin pulled to ground.
    Falling,
    /// Release: pin floats back high.
    Rising,
}

/// The kind of a classified press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressKind {
    /// Released before [`LONG_PRESS_SECONDS`].
    Short,
    /// Released at or after [`LONG_PRESS_SECONDS`], before [`CANCEL_HOLD_SECONDS`].
    Long,
}

impl PressKind {
    /// The mapping-key suffix: `"short"` or `"long"`.
    pub fn as_str(self) -> &'static str {
        match self {
            PressKind::Short => "short",
            PressKind::Long => "long",
        }
    }
}

/// A classified button event, emitted on the release edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ButtonEvent {
    /// The board GPIO number of the button.
    pub pin: u32,
    /// Short or long.
    pub kind: PressKind,
    /// Resolved action name from the live mapping, `None` when unmapped.
    pub action: Option<String>,
    /// Release-edge timestamp, monotonic milliseconds.
    pub timestamp_ms: u64,
}

/// Pure press classifier over synthetic `(pin, edge, ts_ms)` events. Tracks the
/// press timestamp per pin, applies the recent-edge debounce guard, and emits a
/// classified event on the release edge. Holds the live action mapping behind an
/// `Arc<RwLock<_>>` so a swap never lets a release see a half-built map.
pub struct PressClassifier {
    press_ms: HashMap<u32, u64>,
    last_press_ms: HashMap<u32, u64>,
    mapping: Arc<RwLock<HashMap<String, String>>>,
    /// The board's button pins in role order; a pin's slot is its `B<n>` label.
    pins: [u32; 4],
    /// The encoder push-switch line, labelled [`ENC_SW`], when the board has one.
    switch_pin: Option<u32>,
}

impl PressClassifier {
    /// Build a classifier owning `map`, for a board whose buttons are `pins`
    /// (role order). Use [`PressClassifier::mapping_handle`] to swap it later.
    pub fn from_mapping(map: HashMap<String, String>, pins: [u32; 4]) -> Self {
        Self::with_pins(Arc::new(RwLock::new(map)), pins)
    }

    /// Build a classifier sharing an externally-owned mapping handle, for a
    /// board whose buttons are `pins` (role order).
    pub fn with_pins(mapping: Arc<RwLock<HashMap<String, String>>>, pins: [u32; 4]) -> Self {
        Self {
            press_ms: HashMap::new(),
            last_press_ms: HashMap::new(),
            mapping,
            pins,
            switch_pin: None,
        }
    }

    /// Label `pin` as the encoder push switch ([`ENC_SW`]): its presses
    /// classify like a button's and resolve `ENC_SW_short` / `ENC_SW_long`.
    pub fn with_encoder_switch(mut self, pin: u32) -> Self {
        self.switch_pin = Some(pin);
        self
    }

    /// The shared mapping handle. Rebuild and swap the inner map; the next
    /// release reads the new one.
    pub fn mapping_handle(&self) -> Arc<RwLock<HashMap<String, String>>> {
        self.mapping.clone()
    }

    /// The friendly id of `pin` on this board: `ENC_SW` for the encoder switch,
    /// `B1`..`B4` for a panel button, else `BX<pin>`.
    pub fn label(&self, pin: u32) -> String {
        if self.switch_pin == Some(pin) {
            return ENC_SW.to_string();
        }
        pin_label(&self.pins, pin)
    }

    fn resolve_label(&self, label: &str, kind: PressKind) -> Option<String> {
        let key = format!("{label}_{}", kind.as_str());
        self.mapping.read().ok()?.get(&key).cloned()
    }

    fn resolve_action(&self, pin: u32, kind: PressKind) -> Option<String> {
        self.resolve_label(&self.label(pin), kind)
    }

    /// The event of one encoder detent: a `short` press of `ENC_CW` /
    /// `ENC_CCW` on the encoder's A line `pin`, with the action the live
    /// mapping binds to `ENC_CW_short` / `ENC_CCW_short`. A detent has no hold
    /// time, so it is never long and never cancelled.
    pub fn on_rotation(&self, rotation: Rotation, pin: u32, ts_ms: u64) -> ButtonEvent {
        ButtonEvent {
            pin,
            kind: PressKind::Short,
            action: self.resolve_label(rotation.label(), PressKind::Short),
            timestamp_ms: ts_ms,
        }
    }

    /// Feed one edge. Returns `Some(event)` only on a release that classifies as
    /// a non-cancelled press:
    ///
    /// * Falling (press): recorded, unless inside the debounce window of the
    ///   last press on this pin.
    /// * Rising (release): pops the recorded press; a release without a press is
    ///   dropped (spurious edge). Held >= cancel threshold drops; else
    ///   short/long by the long threshold, fired here on release.
    pub fn on_edge(&mut self, pin: u32, edge: Edge, ts_ms: u64) -> Option<ButtonEvent> {
        match edge {
            Edge::Falling => {
                if let Some(&last) = self.last_press_ms.get(&pin) {
                    if ts_ms.saturating_sub(last) < BOUNCE_MS {
                        return None;
                    }
                }
                self.last_press_ms.insert(pin, ts_ms);
                self.press_ms.insert(pin, ts_ms);
                None
            }
            Edge::Rising => {
                let press_ms = self.press_ms.remove(&pin)?;
                let held_ms = ts_ms.saturating_sub(press_ms);
                let held_s = held_ms as f64 / 1000.0;

                if held_s >= CANCEL_HOLD_SECONDS {
                    return None;
                }
                let kind = if held_s >= LONG_PRESS_SECONDS {
                    PressKind::Long
                } else {
                    PressKind::Short
                };
                let action = self.resolve_action(pin, kind);
                Some(ButtonEvent {
                    pin,
                    kind,
                    action,
                    timestamp_ms: ts_ms,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::{Channel, QuadratureDecoder};

    const PINS: [u32; 4] = [5, 6, 13, 19];

    fn fixture_mapping() -> HashMap<String, String> {
        [
            ("B1_short", "cycle_screen"),
            ("B1_long", "toggle_backlight"),
            ("B2_long", "show_qr"),
            ("B3_short", "confirm"),
            ("ENC_CW_short", "cycle_screen"),
            ("ENC_CCW_short", "prev_screen"),
            ("ENC_SW_short", "confirm"),
            ("ENC_SW_long", "back"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
    }

    fn classifier() -> PressClassifier {
        PressClassifier::from_mapping(fixture_mapping(), PINS)
    }

    #[test]
    fn short_press_fires_on_release() {
        let mut c = classifier();
        assert!(c.on_edge(5, Edge::Falling, 1000).is_none());
        let ev = c.on_edge(5, Edge::Rising, 1500).unwrap();
        assert_eq!(ev.pin, 5);
        assert_eq!(ev.kind, PressKind::Short);
        assert_eq!(ev.action.as_deref(), Some("cycle_screen"));
        assert_eq!(ev.timestamp_ms, 1500);
    }

    #[test]
    fn long_press_fires_on_release_not_at_the_mark() {
        let mut c = classifier();
        c.on_edge(6, Edge::Falling, 0);
        let ev = c.on_edge(6, Edge::Rising, 2000).unwrap();
        assert_eq!(ev.kind, PressKind::Long);
        assert_eq!(ev.action.as_deref(), Some("show_qr"));
    }

    #[test]
    fn just_under_long_threshold_is_short() {
        let mut c = classifier();
        c.on_edge(13, Edge::Falling, 0);
        let ev = c.on_edge(13, Edge::Rising, 1999).unwrap();
        assert_eq!(ev.kind, PressKind::Short);
        assert_eq!(ev.action.as_deref(), Some("confirm"));
    }

    #[test]
    fn cancel_hold_drops_the_event() {
        let mut c = classifier();
        c.on_edge(5, Edge::Falling, 0);
        assert!(c.on_edge(5, Edge::Rising, 6000).is_none());
        c.on_edge(5, Edge::Falling, 10_000);
        let ev = c.on_edge(5, Edge::Rising, 15_999).unwrap();
        assert_eq!(ev.kind, PressKind::Long);
    }

    #[test]
    fn debounce_guard_ignores_a_bouncing_press_edge() {
        let mut c = classifier();
        c.on_edge(5, Edge::Falling, 1000);
        // A second press 10 ms later is inside the 20 ms guard -> ignored.
        c.on_edge(5, Edge::Falling, 1010);
        let ev = c.on_edge(5, Edge::Rising, 1300).unwrap();
        assert_eq!(ev.kind, PressKind::Short);
    }

    #[test]
    fn debounce_guard_allows_a_press_after_the_window() {
        let mut c = classifier();
        c.on_edge(5, Edge::Falling, 1000);
        c.on_edge(5, Edge::Rising, 1100);
        assert!(c.on_edge(5, Edge::Falling, 1125).is_none());
        let ev = c.on_edge(5, Edge::Rising, 1200).unwrap();
        assert_eq!(ev.kind, PressKind::Short);
    }

    #[test]
    fn release_without_a_press_is_dropped() {
        let mut c = classifier();
        assert!(c.on_edge(19, Edge::Rising, 500).is_none());
    }

    #[test]
    fn unknown_pin_resolves_bx_label_and_unmapped_action() {
        let mut c = classifier();
        c.on_edge(26, Edge::Falling, 0);
        let ev = c.on_edge(26, Edge::Rising, 100).unwrap();
        assert_eq!(ev.pin, 26);
        assert!(ev.action.is_none());
        assert_eq!(c.label(26), "BX26");
    }

    #[test]
    fn pins_label_by_role_order_not_by_number() {
        let c = PressClassifier::from_mapping(fixture_mapping(), [63, 47, 103, 110]);
        assert_eq!(c.label(63), "B1");
        assert_eq!(c.label(110), "B4");
        assert_eq!(c.label(5), "BX5");
    }

    #[test]
    fn live_mapping_swap_is_seen_by_the_next_release() {
        let mut c = classifier();
        let handle = c.mapping_handle();
        c.on_edge(5, Edge::Falling, 0);
        let ev = c.on_edge(5, Edge::Rising, 100).unwrap();
        assert_eq!(ev.action.as_deref(), Some("cycle_screen"));
        handle
            .write()
            .unwrap()
            .insert("B1_short".into(), "new_action".into());
        c.on_edge(5, Edge::Falling, 1000);
        let ev = c.on_edge(5, Edge::Rising, 1100).unwrap();
        assert_eq!(ev.action.as_deref(), Some("new_action"));
    }

    #[test]
    fn presses_on_distinct_pins_are_tracked_independently() {
        let mut c = classifier();
        c.on_edge(5, Edge::Falling, 0);
        c.on_edge(6, Edge::Falling, 100);
        let e6 = c.on_edge(6, Edge::Rising, 600).unwrap();
        assert_eq!(e6.pin, 6);
        assert_eq!(e6.kind, PressKind::Short);
        let e5 = c.on_edge(5, Edge::Rising, 2500).unwrap();
        assert_eq!(e5.pin, 5);
        assert_eq!(e5.kind, PressKind::Long);
    }

    #[test]
    fn a_detent_is_a_short_press_of_its_rotation_label() {
        let c = classifier();
        let cw = c.on_rotation(Rotation::Cw, 130, 700);
        assert_eq!(cw.pin, 130);
        assert_eq!(cw.kind, PressKind::Short);
        assert_eq!(cw.action.as_deref(), Some("cycle_screen"));
        assert_eq!(cw.timestamp_ms, 700);
        let ccw = c.on_rotation(Rotation::Ccw, 130, 800);
        assert_eq!(ccw.action.as_deref(), Some("prev_screen"));
    }

    #[test]
    fn decoded_detents_resolve_through_the_live_mapping() {
        let c = classifier();
        let mut d = QuadratureDecoder::new(true, true);
        let mut events = Vec::new();
        for (channel, high) in [
            (Channel::A, false),
            (Channel::B, false),
            (Channel::A, true),
            (Channel::B, true),
        ] {
            if let Some(rotation) = d.on_edge(channel, high) {
                events.push((rotation.label(), c.on_rotation(rotation, 130, 1)));
            }
        }
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "ENC_CW");
        assert_eq!(events[0].1.action.as_deref(), Some("cycle_screen"));
        c.mapping_handle()
            .write()
            .unwrap()
            .insert("ENC_CW_short".into(), "show_qr".into());
        let ev = c.on_rotation(Rotation::Cw, 130, 2);
        assert_eq!(ev.action.as_deref(), Some("show_qr"));
    }

    #[test]
    fn the_encoder_switch_is_debounced_and_classified_like_a_button() {
        let mut c = PressClassifier::from_mapping(fixture_mapping(), [42, 41, 44, 128])
            .with_encoder_switch(140);
        assert_eq!(c.label(140), "ENC_SW");
        assert_eq!(c.label(42), "B1");
        assert!(c.on_edge(140, Edge::Falling, 1000).is_none());
        assert!(c.on_edge(140, Edge::Falling, 1008).is_none());
        let ev = c.on_edge(140, Edge::Rising, 1300).unwrap();
        assert_eq!(ev.pin, 140);
        assert_eq!(ev.kind, PressKind::Short);
        assert_eq!(ev.action.as_deref(), Some("confirm"));
        assert!(c.on_edge(140, Edge::Rising, 1400).is_none());
        c.on_edge(140, Edge::Falling, 5000);
        let ev = c.on_edge(140, Edge::Rising, 7500).unwrap();
        assert_eq!(ev.kind, PressKind::Long);
        assert_eq!(ev.action.as_deref(), Some("back"));
        c.on_edge(140, Edge::Falling, 10_000);
        assert!(c.on_edge(140, Edge::Rising, 16_500).is_none());
    }
}
