//! The controller half of the cockpit spec (`web-shared/src/cockpit-spec/ds4.json`),
//! compiled into the binary so the ground station and every operator surface
//! read one table: stick modes, conditioning, the `buttons` bitfield, the
//! evdev maps, the binding rows, the locked actions, the lightbar colours and
//! the rumble patterns.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use serde::Deserialize;

/// The spec file, byte for byte.
pub const DS4_JSON: &str = include_str!("../../../../web-shared/src/cockpit-spec/ds4.json");

/// One stick-mode axis source: which stick, which axis, and whether the sign flips.
#[derive(Debug, Clone, Deserialize)]
pub struct AxisSource {
    pub stick: String,
    pub axis: String,
    pub invert: bool,
}

/// The four pilot axes of one stick mode.
#[derive(Debug, Clone, Deserialize)]
pub struct StickMode {
    pub throttle: AxisSource,
    pub yaw: AxisSource,
    pub pitch: AxisSource,
    pub roll: AxisSource,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Conditioning {
    pub radial_deadzone: f32,
    pub deadzone_min: f32,
    pub deadzone_max: f32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Stream {
    pub rate_hz: u32,
    pub stale_timestamp_ms: u64,
    pub buttons_bitfield: BTreeMap<String, u32>,
    pub trigger_pressed_threshold: f32,
}

/// One binding row (also the row shape of the export file).
#[derive(Debug, Clone, PartialEq, Deserialize, serde::Serialize)]
pub struct Binding {
    pub control: String,
    #[serde(default)]
    pub with: Vec<String>,
    pub gesture: String,
    #[serde(default)]
    pub hold_ms: Option<u64>,
    #[serde(default)]
    pub when: Option<String>,
    pub action: String,
    #[serde(default)]
    pub rumble: Option<String>,
    #[serde(default)]
    pub locked: bool,
}

/// One locked action: it must keep a binding with this gesture (and a
/// modifier combo when `combo`).
#[derive(Debug, Clone, Deserialize)]
pub struct LockedAction {
    pub action: String,
    pub gesture: String,
    pub combo: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LightbarState {
    pub color: String,
    pub pattern: String,
    pub hz: Option<f32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Lightbar {
    pub states: BTreeMap<String, LightbarState>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Pulses {
    pub pulses_ms: Vec<u16>,
    pub gap_ms: u16,
    pub strong: f32,
    pub weak: f32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Ramp {
    pub from: f32,
    pub to: f32,
    pub tick_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Countdown {
    pub pulse_ms: u16,
    pub every_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Rumble {
    pub mode_change: Pulses,
    pub warning: Pulses,
    pub critical: Pulses,
    pub ramp: Ramp,
    pub countdown: Countdown,
}

/// The evdev maps for one kernel driver (names as in `<linux/input-event-codes.h>`).
#[derive(Debug, Clone, Deserialize)]
pub struct EvdevMap {
    pub axes: BTreeMap<String, String>,
    #[serde(default)]
    pub buttons: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LinuxEvdev {
    pub hid_playstation: EvdevMap,
    pub hid_sony_legacy: EvdevMap,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PlatformMap {
    pub linux_evdev: LinuxEvdev,
}

/// The parsed controller spec (the fields the ground station consumes).
#[derive(Debug, Clone, Deserialize)]
pub struct Ds4Spec {
    pub default_mode: String,
    pub stick_modes: BTreeMap<String, StickMode>,
    pub conditioning: Conditioning,
    pub stream: Stream,
    pub centered_threshold: f32,
    pub platform_map: PlatformMap,
    pub bindings: Vec<Binding>,
    pub locked_actions: Vec<LockedAction>,
    pub lightbar: Lightbar,
    pub rumble: Rumble,
}

/// The spec, parsed once. The file is compiled in and covered by a unit test,
/// so a parse failure is a build defect.
pub fn ds4() -> &'static Ds4Spec {
    &SPEC
}

static SPEC: LazyLock<Ds4Spec> =
    LazyLock::new(|| serde_json::from_str(DS4_JSON).expect("cockpit-spec ds4.json parses"));

/// The cockpit layout spec (`web-shared/src/cockpit-spec/layout.json`), for
/// its confirm hold times.
pub const LAYOUT_JSON: &str = include_str!("../../../../web-shared/src/cockpit-spec/layout.json");

/// The emergency stop's confirm hold, `layout.json` `holds_ms.stop`: every
/// binding of the stop is held at least this long before it fires.
pub fn stop_hold_ms() -> u64 {
    *STOP_HOLD_MS
}

static STOP_HOLD_MS: LazyLock<u64> = LazyLock::new(|| {
    let layout: serde_json::Value =
        serde_json::from_str(LAYOUT_JSON).expect("cockpit-spec layout.json parses");
    layout["holds_ms"]["stop"]
        .as_u64()
        .expect("cockpit-spec layout.json holds_ms.stop is a whole number of ms")
});

/// An evdev ABS/KEY name from the spec onto its code.
pub fn evdev_code(name: &str) -> Option<u16> {
    Some(match name {
        "ABS_X" => 0x00,
        "ABS_Y" => 0x01,
        "ABS_Z" => 0x02,
        "ABS_RX" => 0x03,
        "ABS_RY" => 0x04,
        "ABS_RZ" => 0x05,
        "ABS_HAT0X" => 0x10,
        "ABS_HAT0Y" => 0x11,
        "ABS_HAT2X" => 0x14,
        "ABS_HAT2Y" => 0x15,
        "BTN_SOUTH" => 0x130,
        "BTN_EAST" => 0x131,
        "BTN_NORTH" => 0x133,
        "BTN_WEST" => 0x134,
        "BTN_TL" => 0x136,
        "BTN_TR" => 0x137,
        "BTN_TL2" => 0x138,
        "BTN_TR2" => 0x139,
        "BTN_SELECT" => 0x13A,
        "BTN_START" => 0x13B,
        "BTN_MODE" => 0x13C,
        "BTN_THUMBL" => 0x13D,
        "BTN_THUMBR" => 0x13E,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_parses_and_every_evdev_name_resolves() {
        let s = ds4();
        assert_eq!(s.stream.rate_hz, 50);
        assert!(s.stick_modes.contains_key("mode1") && s.stick_modes.contains_key("mode2"));
        for map in [
            &s.platform_map.linux_evdev.hid_playstation,
            &s.platform_map.linux_evdev.hid_sony_legacy,
        ] {
            for name in map.axes.values().chain(map.buttons.values()) {
                assert!(evdev_code(name).is_some(), "unmapped evdev name {name}");
            }
        }
        for state in [
            "disarmed",
            "armed_idle",
            "pilot",
            "skill",
            "mission",
            "rth",
            "low_battery",
            "failsafe",
        ] {
            assert!(
                s.lightbar.states.contains_key(state),
                "lightbar state {state}"
            );
        }
        assert!(!s.locked_actions.is_empty());
    }
}
