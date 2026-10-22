//! Front-panel button service.
//!
//! Reads the four front-panel buttons the board binds (see [`board`]) over the
//! character-device GPIO interface and classifies each press as short / long /
//! cancel on the RELEASE edge. This is the only implementation: it carries the
//! thresholds, the pin->label table, the default mapping, the SIGHUP-rebuilt
//! action mapping merged over the defaults, and the skip-clean posture when the
//! board has no buttons to read.
//!
//! A board that declares a rotary encoder is read by the same loop: each
//! detent is a `short` press of `ENC_CW` / `ENC_CCW`, and the push switch is
//! classified like a button under `ENC_SW`. Their actions resolve through the
//! same mapping (`ENC_CW_short`, `ENC_CCW_short`, `ENC_SW_short`,
//! `ENC_SW_long`). The pure classification and quadrature decoding live in
//! `ados_hmi::press` and `ados_hmi::encoder`; only the chip-open + edge-event
//! read loop and the profile mapping live here.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

pub mod board;

use ados_hmi::press::PressClassifier;

pub use board::{
    board_button_pins, board_encoder_switch, board_pins_or_default, button_lines, encoder_lines,
    ButtonLine, ButtonsSkip, EncoderLines, EncoderSkip, GpioNumbering, BOARD_SIDECAR, BUTTON_PINS,
};

/// A classifier seeded with the default mapping of `profile`, labelling the
/// reference-panel pins.
pub fn classifier_for_profile(profile: &str) -> PressClassifier {
    PressClassifier::from_mapping(default_button_mapping(profile), BUTTON_PINS)
}

/// A classifier sharing an externally-owned mapping handle (so the daemon's
/// SIGHUP task can swap it), labelling the reference-panel pins.
pub fn classifier_with_mapping(mapping: Arc<RwLock<HashMap<String, String>>>) -> PressClassifier {
    PressClassifier::with_pins(mapping, BUTTON_PINS)
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

#[cfg(target_os = "linux")]
mod reader;
#[cfg(target_os = "linux")]
pub use reader::run_event_loop;

#[cfg(test)]
mod tests {
    use super::*;
    use ados_hmi::encoder::{Rotation, ENC_CCW, ENC_CW, ENC_SW};
    use ados_hmi::press::Edge;

    #[test]
    fn profile_classifier_labels_the_reference_panel_pins() {
        let mut c = classifier_for_profile("ground_station");
        c.on_edge(5, Edge::Falling, 1000);
        let ev = c.on_edge(5, Edge::Rising, 1500).unwrap();
        assert_eq!(ev.action.as_deref(), Some("cycle_screen")); // B1_short
        c.on_edge(6, Edge::Falling, 2000);
        let ev = c.on_edge(6, Edge::Rising, 4000).unwrap();
        assert_eq!(ev.action.as_deref(), Some("show_qr")); // B2_long
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
        let mut c = classifier_for_profile("visor");
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
        let mut c = classifier_with_mapping(handle.clone());
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
    fn default_encoder_mapping_resolves_detents() {
        let c = classifier_for_profile("ground_station");
        let ccw = c.on_rotation(Rotation::Ccw, 130, 800);
        assert_eq!(ccw.action.as_deref(), Some("prev_screen"));
    }
}
