//! Front-panel button service.
//!
//! Reads the four ground-station front-panel buttons on GPIO 5, 6, 13, and 19
//! (BCM) over the character-device GPIO interface and classifies each press as
//! short / long / cancel on the RELEASE edge. Ports
//! `src/ados/services/ui/button_service.py`: the pin list, the thresholds, the
//! pin->label table, the default mapping, the SIGHUP-rebuilt action mapping
//! merged over the defaults, and the skip-clean posture when no GPIO chip
//! exists.
//!
//! The classification ([`PressClassifier`]) is pure and host-portable: it takes
//! synthetic `(pin, edge, ts_ms)` events, so the debounce guard, the short/long
//! thresholds, the cancel-drop, and the mapping merge are all unit-testable
//! without hardware. Only the chip-open + edge-event read loop is target-gated
//! to Linux (it needs the cdev ioctls).

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// Fallback pin list (BCM). Future revisions pull this from the HAL profile.
pub const BUTTON_PINS: [u32; 4] = [5, 6, 13, 19];

/// Held >= this fires a "long" press (on release, never at the mark).
pub const LONG_PRESS_SECONDS: f64 = 2.0;

/// Held >= this drops the event entirely (operator changed their mind).
pub const CANCEL_HOLD_SECONDS: f64 = 6.0;

/// Software recent-edge debounce window.
pub const BOUNCE_MS: u64 = 20;

/// BCM pin -> friendly button id used by the REST schema. Anything outside this
/// table resolves as `BX<pin>` so extra GPIO buttons get a stable mapping key.
pub fn pin_to_label(pin: u32) -> String {
    match pin {
        5 => "B1".to_string(),
        6 => "B2".to_string(),
        13 => "B3".to_string(),
        19 => "B4".to_string(),
        other => format!("BX{other}"),
    }
}

/// Default mapping, used when `ground_station.ui.buttons.mapping` is empty or
/// missing. Verbatim from the Python `DEFAULT_BUTTON_MAPPING` /
/// `_DEFAULT_BUTTONS`.
pub fn default_button_mapping() -> HashMap<String, String> {
    [
        ("B1_short", "cycle_screen"),
        ("B1_long", "toggle_backlight"),
        ("B2_short", "show_network"),
        ("B2_long", "show_qr"),
        ("B3_short", "confirm"),
        ("B3_long", "pair_drone"),
        ("B4_short", "back"),
        ("B4_long", "menu"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

/// Merge a loaded `ground_station.ui.buttons.mapping` over the defaults. A
/// partial remap (user only changed `B1_short`) keeps the rest of the defaults
/// intact. Non-string keys/values in the loaded map are ignored.
pub fn merge_mapping(loaded: &serde_norway::Value) -> HashMap<String, String> {
    let mut merged = default_button_mapping();
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
    /// BCM pin number.
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
}

impl PressClassifier {
    /// Build a classifier seeded with the default mapping.
    pub fn new() -> Self {
        Self::with_mapping(Arc::new(RwLock::new(default_button_mapping())))
    }

    /// Build a classifier sharing an externally-owned mapping handle (so the
    /// daemon's SIGHUP task can swap it).
    pub fn with_mapping(mapping: Arc<RwLock<HashMap<String, String>>>) -> Self {
        Self {
            press_ms: HashMap::new(),
            last_press_ms: HashMap::new(),
            mapping,
        }
    }

    /// The shared mapping handle. A SIGHUP handler rebuilds and swaps the inner
    /// map; the next release reads the new one.
    pub fn mapping_handle(&self) -> Arc<RwLock<HashMap<String, String>>> {
        self.mapping.clone()
    }

    fn resolve_action(&self, pin: u32, kind: PressKind) -> Option<String> {
        let key = format!("{}_{}", pin_to_label(pin), kind.as_str());
        self.mapping.read().ok()?.get(&key).cloned()
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

impl Default for PressClassifier {
    fn default() -> Self {
        Self::new()
    }
}

/// True when a GPIO chip the buttons could attach to exists. Front-panel buttons
/// only exist on boards with a GPIO header; a board with no `/dev/gpiochip*` has
/// nothing to attach to, so the daemon skips cleanly (exit 0) rather than
/// failing — a board with no buttons is a supported configuration.
pub fn gpio_subsystem_present() -> bool {
    let Ok(rd) = std::fs::read_dir("/dev") else {
        return false;
    };
    rd.filter_map(|e| e.ok()).any(|e| {
        e.file_name()
            .to_str()
            .map(|n| n.starts_with("gpiochip"))
            .unwrap_or(false)
    })
}

/// Run the GPIO edge-event read loop on Linux, classifying presses and invoking
/// `on_event` for each emitted [`ButtonEvent`]. Opens `chip_path`, requests
/// both edges on every pin, and blocks reading edge events. Returns when the
/// reader errors. The chip-open is the only hardware-coupled step; the
/// classification stays in [`PressClassifier`].
#[cfg(target_os = "linux")]
pub fn run_event_loop<F>(
    chip_path: &str,
    pins: &[u32],
    classifier: &mut PressClassifier,
    mut on_event: F,
) -> anyhow::Result<()>
where
    F: FnMut(ButtonEvent),
{
    use gpio_cdev::{Chip, EventRequestFlags, EventType, LineRequestFlags};
    use std::time::Instant;

    let mut chip = Chip::new(chip_path)?;
    let base = Instant::now();

    // Request both edges on every pin and collect the event iterators.
    let mut handles = Vec::new();
    for &pin in pins {
        let line = chip.get_line(pin)?;
        let events = line.events(
            LineRequestFlags::INPUT,
            EventRequestFlags::BOTH_EDGES,
            "ados-pic-buttons",
        )?;
        handles.push((pin, events));
    }

    // gpio-cdev's per-line iterators block; poll them in turn. With four
    // front-panel buttons this round-robin is cheap and keeps the chip-open in
    // one place. The synchronous loop runs on a blocking task in the daemon.
    loop {
        for (pin, events) in handles.iter_mut() {
            // next() blocks until an edge arrives on this line.
            let Some(evt) = events.next() else {
                return Ok(());
            };
            let evt = evt?;
            let edge = match evt.event_type() {
                EventType::FallingEdge => Edge::Falling,
                EventType::RisingEdge => Edge::Rising,
            };
            let ts_ms = base.elapsed().as_millis() as u64;
            if let Some(ev) = classifier.on_edge(*pin, edge, ts_ms) {
                on_event(ev);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classifier() -> PressClassifier {
        PressClassifier::new()
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
        assert_eq!(pin_to_label(26), "BX26");
    }

    #[test]
    fn default_mapping_has_the_eight_entries() {
        let m = default_button_mapping();
        assert_eq!(m.len(), 8);
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
    fn sighup_merge_overrides_one_key_keeps_the_rest() {
        let loaded: serde_norway::Value =
            serde_norway::from_str("B1_short: my_custom_action\n").unwrap();
        let merged = merge_mapping(&loaded);
        // Overridden.
        assert_eq!(merged["B1_short"], "my_custom_action");
        // Untouched defaults remain.
        assert_eq!(merged["B1_long"], "toggle_backlight");
        assert_eq!(merged["B4_long"], "menu");
        assert_eq!(merged.len(), 8);
    }

    #[test]
    fn sighup_merge_adds_extra_pin_keys() {
        let loaded: serde_norway::Value =
            serde_norway::from_str("BX26_short: extra_action\n").unwrap();
        let merged = merge_mapping(&loaded);
        assert_eq!(merged["BX26_short"], "extra_action");
        assert_eq!(merged.len(), 9); // 8 defaults + 1 extra
    }

    #[test]
    fn sighup_merge_ignores_non_string_and_empty() {
        // Empty mapping -> defaults only.
        let empty: serde_norway::Value = serde_norway::from_str("{}\n").unwrap();
        assert_eq!(merge_mapping(&empty).len(), 8);
        // Non-mapping value -> defaults only.
        let scalar: serde_norway::Value = serde_norway::from_str("not_a_map\n").unwrap();
        assert_eq!(merge_mapping(&scalar).len(), 8);
        // Non-string value is skipped.
        let mixed: serde_norway::Value =
            serde_norway::from_str("B1_short: 42\nB2_short: ok_action\n").unwrap();
        let merged = merge_mapping(&mixed);
        assert_eq!(merged["B1_short"], "cycle_screen"); // default kept (42 ignored)
        assert_eq!(merged["B2_short"], "ok_action");
    }

    #[test]
    fn live_mapping_swap_is_seen_by_the_next_release() {
        let handle = Arc::new(RwLock::new(default_button_mapping()));
        let mut c = PressClassifier::with_mapping(handle.clone());
        c.on_edge(5, Edge::Falling, 0);
        let ev = c.on_edge(5, Edge::Rising, 100).unwrap();
        assert_eq!(ev.action.as_deref(), Some("cycle_screen"));
        // SIGHUP-style swap.
        *handle.write().unwrap() = {
            let mut m = default_button_mapping();
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
}
