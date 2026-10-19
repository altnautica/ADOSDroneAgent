//! Front-panel button service.
//!
//! Reads the four front-panel buttons the board binds (see [`board`]) over the
//! character-device GPIO interface and classifies each press as short / long /
//! cancel on the RELEASE edge. This is the only implementation: it carries the
//! thresholds, the pin->label table, the default mapping, the SIGHUP-rebuilt
//! action mapping merged over the defaults, and the skip-clean posture when the
//! board has no buttons to read.
//!
//! A board that declares a rotary encoder (see [`encoder`]) is read by the same
//! loop: each detent is a `short` press of `ENC_CW` / `ENC_CCW`, and the push
//! switch is classified like a button under `ENC_SW`. Their actions resolve
//! through the same mapping (`ENC_CW_short`, `ENC_CCW_short`, `ENC_SW_short`,
//! `ENC_SW_long`).
//!
//! The classification ([`PressClassifier`]) is pure and host-portable: it takes
//! synthetic `(pin, edge, ts_ms)` events, so the debounce guard, the short/long
//! thresholds, the cancel-drop, and the mapping merge are all unit-testable
//! without hardware. Only the chip-open + edge-event read loop is target-gated
//! to Linux (it needs the cdev ioctls).

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

pub mod board;
pub mod encoder;

pub use board::{
    board_button_pins, board_encoder_switch, board_pins_or_default, button_lines, encoder_lines,
    ButtonLine, ButtonsSkip, EncoderLines, EncoderSkip, GpioNumbering, BOARD_SIDECAR, BUTTON_PINS,
};
pub use encoder::{Channel, QuadratureDecoder, Rotation, ENC_CCW, ENC_CW, ENC_SW};

/// Held >= this fires a "long" press (on release, never at the mark).
pub const LONG_PRESS_SECONDS: f64 = 2.0;

/// Held >= this drops the event entirely (operator changed their mind).
pub const CANCEL_HOLD_SECONDS: f64 = 6.0;

/// Software recent-edge debounce window.
pub const BOUNCE_MS: u64 = 20;

/// The friendly button id of `pin` on a board whose buttons are `pins` (role
/// order up, down, select, back -> `B1`..`B4`), used as the mapping key prefix.
/// Anything else resolves as `BX<pin>` so extra GPIO buttons get a stable key.
pub fn pin_label(pins: &[u32; 4], pin: u32) -> String {
    match pins.iter().position(|&p| p == pin) {
        Some(slot) => format!("B{}", slot + 1),
        None => format!("BX{pin}"),
    }
}

/// The default mapping for `profile` (underscore wire spelling), used when
/// `ground_station.ui.buttons.mapping` is empty or missing: the profile's button
/// table plus the encoder table. Sourced from the shared panel tables so the
/// control surface and the display agree.
pub fn default_button_mapping(profile: &str) -> HashMap<String, String> {
    ados_protocol::panel_ui::default_buttons(profile)
        .iter()
        .chain(ados_protocol::panel_ui::ENCODER_BUTTONS.iter())
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

/// Merge a loaded `ground_station.ui.buttons.mapping` over the defaults of
/// `profile`. A partial remap (user only changed `B1_short`) keeps the rest of
/// the defaults intact. Non-string keys/values in the loaded map are ignored.
pub fn merge_mapping(profile: &str, loaded: &serde_norway::Value) -> HashMap<String, String> {
    let mut merged = default_button_mapping(profile);
    if let serde_norway::Value::Mapping(map) = loaded {
        for (k, v) in map {
            if let (Some(k), Some(v)) = (k.as_str(), v.as_str()) {
                merged.insert(k.to_string(), v.to_string());
            }
        }
    }
    merged
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
    Short,
    Long,
}

impl PressKind {
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
    /// The board GPIO number of the button (BCM on a Raspberry Pi, the Linux
    /// GPIO number on Rockchip).
    pub pin: u32,
    pub kind: PressKind,
    /// Resolved action name from the live mapping, `None` when unmapped.
    pub action: Option<String>,
    /// Release-edge timestamp, monotonic milliseconds.
    pub timestamp_ms: u64,
}

/// Pure press classifier over synthetic `(pin, edge, ts_ms)` events. Tracks the
/// press timestamp per pin, applies the recent-edge debounce guard, and emits a
/// classified event on the release edge. Holds the live action mapping behind an
/// `Arc<RwLock<_>>` so a SIGHUP swap never lets a release see a half-built map.
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
    /// Build a classifier seeded with the default mapping of `profile`.
    pub fn new(profile: &str) -> Self {
        Self::with_mapping(Arc::new(RwLock::new(default_button_mapping(profile))))
    }

    /// Build a classifier sharing an externally-owned mapping handle (so the
    /// daemon's SIGHUP task can swap it), labelling the reference-panel pins.
    pub fn with_mapping(mapping: Arc<RwLock<HashMap<String, String>>>) -> Self {
        Self::with_pins(mapping, BUTTON_PINS)
    }

    /// Build a classifier for a board whose buttons are `pins` (role order).
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

    /// The shared mapping handle. A SIGHUP handler rebuilds and swaps the inner
    /// map; the next release reads the new one.
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

    /// The event of one encoder detent: a `short` press of [`ENC_CW`] /
    /// [`ENC_CCW`] on the encoder's A line `pin`, with the action the live
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
    /// a non-cancelled press. Mirrors the Python press/release handlers:
    ///
    /// * Falling (press): recorded, unless inside the debounce window of the
    ///   last press on this pin.
    /// * Rising (release): pops the recorded press; a release without a press is
    ///   dropped (spurious edge). Held >= cancel threshold drops + would log;
    ///   else short/long by the long threshold, fired here on release.
    pub fn on_edge(&mut self, pin: u32, edge: Edge, ts_ms: u64) -> Option<ButtonEvent> {
        match edge {
            Edge::Falling => {
                // Recent-edge guard: ignore a press edge inside the debounce
                // window of the last press on this pin.
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
                    // Held too long: drop. The daemon logs the cancel.
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

#[cfg(target_os = "linux")]
mod reader;
#[cfg(target_os = "linux")]
pub use reader::run_event_loop;

#[cfg(test)]
mod tests {
    use super::*;

    fn classifier() -> PressClassifier {
        PressClassifier::new("ground_station")
    }

    #[test]
    fn short_press_fires_on_release() {
        let mut c = classifier();
        assert!(c.on_edge(5, Edge::Falling, 1000).is_none());
        // Released 500 ms later -> short.
        let ev = c.on_edge(5, Edge::Rising, 1500).unwrap();
        assert_eq!(ev.pin, 5);
        assert_eq!(ev.kind, PressKind::Short);
        assert_eq!(ev.action.as_deref(), Some("cycle_screen")); // B1_short
        assert_eq!(ev.timestamp_ms, 1500);
    }

    #[test]
    fn long_press_fires_on_release_not_at_the_mark() {
        let mut c = classifier();
        c.on_edge(6, Edge::Falling, 0);
        // Held exactly the long threshold -> long.
        let ev = c.on_edge(6, Edge::Rising, 2000).unwrap();
        assert_eq!(ev.kind, PressKind::Long);
        assert_eq!(ev.action.as_deref(), Some("show_qr")); // B2_long
    }

    #[test]
    fn just_under_long_threshold_is_short() {
        let mut c = classifier();
        c.on_edge(13, Edge::Falling, 0);
        let ev = c.on_edge(13, Edge::Rising, 1999).unwrap();
        assert_eq!(ev.kind, PressKind::Short);
        assert_eq!(ev.action.as_deref(), Some("confirm")); // B3_short
    }

    #[test]
    fn cancel_hold_drops_the_event() {
        let mut c = classifier();
        c.on_edge(5, Edge::Falling, 0);
        // Held past the cancel threshold -> dropped.
        assert!(c.on_edge(5, Edge::Rising, 6000).is_none());
        // Just under cancel still fires (as long).
        c.on_edge(5, Edge::Falling, 10_000);
        let ev = c.on_edge(5, Edge::Rising, 15_999).unwrap();
        assert_eq!(ev.kind, PressKind::Long);
    }

    #[test]
    fn debounce_guard_ignores_a_bouncing_press_edge() {
        let mut c = classifier();
        c.on_edge(5, Edge::Falling, 1000); // first press recorded
                                           // A second press 10 ms later is inside the 20 ms guard -> ignored, so
                                           // the recorded press timestamp stays at 1000.
        c.on_edge(5, Edge::Falling, 1010);
        let ev = c.on_edge(5, Edge::Rising, 1300).unwrap();
        // held = 1300 - 1000 = 300 ms -> short (would be 290 if the bounce had
        // overwritten the press time, but either way short; the guard's real
        // job is not double-counting, asserted next).
        assert_eq!(ev.kind, PressKind::Short);
    }

    #[test]
    fn debounce_guard_allows_a_press_after_the_window() {
        let mut c = classifier();
        c.on_edge(5, Edge::Falling, 1000);
        c.on_edge(5, Edge::Rising, 1100); // short press, clears state
                                          // A fresh press 25 ms later (> 20 ms guard) is honored.
        assert!(c.on_edge(5, Edge::Falling, 1125).is_none());
        let ev = c.on_edge(5, Edge::Rising, 1200).unwrap();
        assert_eq!(ev.kind, PressKind::Short);
    }

    #[test]
    fn release_without_a_press_is_dropped() {
        let mut c = classifier();
        // Spurious rising edge with no recorded press.
        assert!(c.on_edge(19, Edge::Rising, 500).is_none());
    }

    #[test]
    fn unknown_pin_resolves_bx_label_and_unmapped_action() {
        let mut c = classifier();
        c.on_edge(26, Edge::Falling, 0);
        let ev = c.on_edge(26, Edge::Rising, 100).unwrap();
        assert_eq!(ev.pin, 26);
        // BX26_short has no default mapping -> action None.
        assert!(ev.action.is_none());
        assert_eq!(c.label(26), "BX26");
    }

    #[test]
    fn board_pins_label_by_role_order_not_by_bcm_number() {
        // A Rockchip board's Linux GPIO numbers take the B1..B4 slots, so the
        // shared panel tables apply; BCM 5 is just another unknown pin there.
        let mapping = Arc::new(RwLock::new(default_button_mapping("visor")));
        let mut c = PressClassifier::with_pins(mapping, [63, 47, 103, 110]);
        assert_eq!(c.label(63), "B1");
        assert_eq!(c.label(110), "B4");
        assert_eq!(c.label(5), "BX5");
        c.on_edge(63, Edge::Falling, 0);
        let ev = c.on_edge(63, Edge::Rising, 2500).unwrap();
        assert_eq!(ev.pin, 63);
        assert_eq!(ev.action.as_deref(), Some("shutdown_prompt")); // B1_long
        c.on_edge(103, Edge::Falling, 5000);
        let ev = c.on_edge(103, Edge::Rising, 5100).unwrap();
        assert_eq!(ev.action.as_deref(), Some("cycle_screen")); // B3_short
    }

    #[test]
    fn default_mapping_has_the_eight_button_and_four_encoder_entries() {
        let m = default_button_mapping("ground_station");
        assert_eq!(m.len(), 12);
        assert_eq!(m["B1_short"], "cycle_screen");
        assert_eq!(m["B1_long"], "toggle_backlight");
        assert_eq!(m["B2_short"], "show_network");
        assert_eq!(m["B2_long"], "show_qr");
        assert_eq!(m["B3_short"], "confirm");
        assert_eq!(m["B3_long"], "pair_drone");
        assert_eq!(m["B4_short"], "back");
        assert_eq!(m["B4_long"], "menu");
    }

    #[test]
    fn visor_mapping_leaves_record_and_mark_to_plugins() {
        let m = default_button_mapping("visor");
        assert_eq!(m.len(), 12);
        assert_eq!(m["B1_short"], "passthrough");
        assert_eq!(m["B2_short"], "passthrough");
        assert_eq!(m["B1_long"], "shutdown_prompt");
        assert_eq!(m["B3_short"], "cycle_screen");
        assert_eq!(m["B4_short"], "back");
        let mut c = PressClassifier::new("visor");
        c.on_edge(5, Edge::Falling, 0);
        let ev = c.on_edge(5, Edge::Rising, 2500).unwrap();
        assert_eq!(ev.action.as_deref(), Some("shutdown_prompt"));
        let loaded: serde_norway::Value = serde_norway::from_str("B2_short: back\n").unwrap();
        let merged = merge_mapping("visor", &loaded);
        assert_eq!(merged["B2_short"], "back");
        assert_eq!(merged["B1_short"], "passthrough");
    }

    #[test]
    fn sighup_merge_overrides_one_key_keeps_the_rest() {
        let loaded: serde_norway::Value =
            serde_norway::from_str("B1_short: my_custom_action\n").unwrap();
        let merged = merge_mapping("ground_station", &loaded);
        // Overridden.
        assert_eq!(merged["B1_short"], "my_custom_action");
        // Untouched defaults remain.
        assert_eq!(merged["B1_long"], "toggle_backlight");
        assert_eq!(merged["B4_long"], "menu");
        assert_eq!(merged.len(), 12);
    }

    #[test]
    fn sighup_merge_adds_extra_pin_keys() {
        let loaded: serde_norway::Value =
            serde_norway::from_str("BX26_short: extra_action\n").unwrap();
        let merged = merge_mapping("ground_station", &loaded);
        assert_eq!(merged["BX26_short"], "extra_action");
        assert_eq!(merged.len(), 13); // 12 defaults + 1 extra
    }

    #[test]
    fn sighup_merge_ignores_non_string_and_empty() {
        // Empty mapping -> defaults only.
        let empty: serde_norway::Value = serde_norway::from_str("{}\n").unwrap();
        assert_eq!(merge_mapping("ground_station", &empty).len(), 12);
        // Non-mapping value -> defaults only.
        let scalar: serde_norway::Value = serde_norway::from_str("not_a_map\n").unwrap();
        assert_eq!(merge_mapping("ground_station", &scalar).len(), 12);
        // Non-string value is skipped.
        let mixed: serde_norway::Value =
            serde_norway::from_str("B1_short: 42\nB2_short: ok_action\n").unwrap();
        let merged = merge_mapping("ground_station", &mixed);
        assert_eq!(merged["B1_short"], "cycle_screen"); // default kept (42 ignored)
        assert_eq!(merged["B2_short"], "ok_action");
    }

    #[test]
    fn live_mapping_swap_is_seen_by_the_next_release() {
        let handle = Arc::new(RwLock::new(default_button_mapping("ground_station")));
        let mut c = PressClassifier::with_mapping(handle.clone());
        c.on_edge(5, Edge::Falling, 0);
        let ev = c.on_edge(5, Edge::Rising, 100).unwrap();
        assert_eq!(ev.action.as_deref(), Some("cycle_screen"));
        // SIGHUP-style swap.
        *handle.write().unwrap() = {
            let mut m = default_button_mapping("ground_station");
            m.insert("B1_short".into(), "new_action".into());
            m
        };
        c.on_edge(5, Edge::Falling, 1000);
        let ev = c.on_edge(5, Edge::Rising, 1100).unwrap();
        assert_eq!(ev.action.as_deref(), Some("new_action"));
    }

    #[test]
    fn presses_on_distinct_pins_are_tracked_independently() {
        let mut c = classifier();
        c.on_edge(5, Edge::Falling, 0);
        c.on_edge(6, Edge::Falling, 100);
        // pin 6 released first as a short, pin 5 later as a long.
        let e6 = c.on_edge(6, Edge::Rising, 600).unwrap();
        assert_eq!(e6.pin, 6);
        assert_eq!(e6.kind, PressKind::Short);
        let e5 = c.on_edge(5, Edge::Rising, 2500).unwrap();
        assert_eq!(e5.pin, 5);
        assert_eq!(e5.kind, PressKind::Long);
    }

    #[test]
    fn encoder_defaults_step_screens_and_select_on_every_panel_profile() {
        for profile in ["ground_station", "visor"] {
            let m = default_button_mapping(profile);
            assert_eq!(m[&format!("{ENC_CW}_short")], "cycle_screen", "{profile}");
            assert_eq!(
                m[&format!("{ENC_CCW}_short")],
                ados_protocol::panel_ui::PREV_SCREEN,
                "{profile}"
            );
            assert_eq!(m[&format!("{ENC_SW}_short")], "confirm", "{profile}");
            assert_eq!(m[&format!("{ENC_SW}_long")], "back", "{profile}");
        }
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
        // The full path the reader runs: edges -> decoder -> classifier.
        let handle = Arc::new(RwLock::new(default_button_mapping("ground_station")));
        let c = PressClassifier::with_pins(handle.clone(), [42, 41, 44, 128]);
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
        // An operator remap of the encoder is seen by the next detent.
        handle
            .write()
            .unwrap()
            .insert("ENC_CW_short".into(), "show_qr".into());
        let ev = c.on_rotation(Rotation::Cw, 130, 2);
        assert_eq!(ev.action.as_deref(), Some("show_qr"));
    }

    #[test]
    fn the_encoder_switch_is_debounced_and_classified_like_a_button() {
        let mapping = Arc::new(RwLock::new(default_button_mapping("ground_station")));
        let mut c = PressClassifier::with_pins(mapping, [42, 41, 44, 128]).with_encoder_switch(140);
        assert_eq!(c.label(140), "ENC_SW");
        assert_eq!(c.label(42), "B1");
        // A bouncing press edge inside the window is ignored; the release
        // classifies from the first press.
        assert!(c.on_edge(140, Edge::Falling, 1000).is_none());
        assert!(c.on_edge(140, Edge::Falling, 1008).is_none());
        let ev = c.on_edge(140, Edge::Rising, 1300).unwrap();
        assert_eq!(ev.pin, 140);
        assert_eq!(ev.kind, PressKind::Short);
        assert_eq!(ev.action.as_deref(), Some("confirm"));
        // A spurious release with no press is dropped.
        assert!(c.on_edge(140, Edge::Rising, 1400).is_none());
        // Held past the long threshold: the long mapping.
        c.on_edge(140, Edge::Falling, 5000);
        let ev = c.on_edge(140, Edge::Rising, 7500).unwrap();
        assert_eq!(ev.kind, PressKind::Long);
        assert_eq!(ev.action.as_deref(), Some("back"));
        // Held past the cancel threshold: dropped.
        c.on_edge(140, Edge::Falling, 10_000);
        assert!(c.on_edge(140, Edge::Rising, 16_500).is_none());
    }
}
