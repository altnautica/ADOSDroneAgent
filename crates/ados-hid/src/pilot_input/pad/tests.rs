//! evdev fixtures for both DS4 kernel drivers.

use super::*;

const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_Z: u16 = 0x02;
const ABS_RX: u16 = 0x03;
const ABS_RY: u16 = 0x04;
const ABS_RZ: u16 = 0x05;
const ABS_HAT0X: u16 = 0x10;
const ABS_HAT0Y: u16 = 0x11;
const ABS_HAT2X: u16 = 0x14;
const ABS_HAT2Y: u16 = 0x15;
const BTN_SOUTH: u16 = 0x130;
const BTN_MODE: u16 = 0x13C;
const BTN_TL: u16 = 0x136;

fn ax(value: i32) -> AbsAxis {
    AbsAxis { value, min: 0, max: 255 }
}

fn hat(value: i32) -> AbsAxis {
    AbsAxis { value, min: -1, max: 1 }
}

/// `hid-playstation` DS4: sticks 0..255, triggers ABS_Z/RZ 0..255, D-pad on HAT0.
/// Left stick pushed full up, right stick full right, R2 fully pulled, D-pad up,
/// Cross and PS held.
fn playstation_fixture() -> RawPad {
    let mut p = RawPad::default();
    p.abs.insert(ABS_X, ax(128));
    p.abs.insert(ABS_Y, ax(0));
    p.abs.insert(ABS_RX, ax(255));
    p.abs.insert(ABS_RY, ax(128));
    p.abs.insert(ABS_Z, ax(0));
    p.abs.insert(ABS_RZ, ax(255));
    p.abs.insert(ABS_HAT0X, hat(0));
    p.abs.insert(ABS_HAT0Y, hat(-1));
    p.keys.insert(BTN_SOUTH);
    p.keys.insert(BTN_MODE);
    p
}

/// Legacy `hid-sony`: no ABS_RX; right stick on ABS_Z/RZ, triggers on HAT2Y (L2)
/// and HAT2X (R2). Right stick full forward, L2 fully pulled, L1 held.
fn legacy_fixture() -> RawPad {
    let mut p = RawPad::default();
    p.abs.insert(ABS_X, ax(128));
    p.abs.insert(ABS_Y, ax(128));
    p.abs.insert(ABS_Z, ax(128));
    p.abs.insert(ABS_RZ, ax(0));
    p.abs.insert(ABS_HAT2Y, ax(255));
    p.abs.insert(ABS_HAT2X, ax(0));
    p.keys.insert(BTN_TL);
    p
}

fn mode2() -> &'static StickMode {
    &ds4().stick_modes["mode2"]
}

#[test]
fn detection_follows_abs_rx() {
    assert_eq!(Driver::detect(true), Driver::HidPlaystation);
    assert_eq!(Driver::detect(false), Driver::HidSonyLegacy);
}

#[test]
fn playstation_fixture_maps_sticks_triggers_and_buttons() {
    let map = PadMap::for_driver(Driver::HidPlaystation);
    let st = map.read(&playstation_fixture(), 0.5);
    let axes = pilot_axes(&st, mode2(), 0.08);
    // Left stick up = climb (thr +1), right stick right = roll +1.
    assert!((axes[0] - 1.0).abs() < 0.01, "roll {axes:?}");
    assert!(axes[1].abs() < 0.01, "pitch {axes:?}");
    assert!((axes[2] - 1.0).abs() < 0.01, "thr {axes:?}");
    assert!(axes[3].abs() < 0.01, "yaw {axes:?}");
    assert!(st.pressed("cross"));
    assert!(st.pressed("ps"));
    assert!(st.pressed("dpad_up"));
    assert!(st.pressed("r2"), "R2 analog over threshold sets its bit");
    assert!(!st.pressed("l2"));
    assert!((st.r2 - 1.0).abs() < 1e-6);
    // Bits per the spec bitfield.
    assert_eq!(st.buttons & 1, 1, "cross is bit 0");
    assert_ne!(st.buttons & (1 << 16), 0, "ps is bit 16");
    assert_ne!(st.buttons & (1 << 12), 0, "dpad_up is bit 12");
}

#[test]
fn legacy_fixture_reads_right_stick_on_z_rz_and_triggers_on_hat2() {
    let map = PadMap::for_driver(Driver::HidSonyLegacy);
    let st = map.read(&legacy_fixture(), 0.5);
    let axes = pilot_axes(&st, mode2(), 0.08);
    assert!((axes[1] - 1.0).abs() < 0.01, "right stick forward = pitch +1: {axes:?}");
    assert!(axes[2].abs() < 0.01, "left stick centred: {axes:?}");
    assert!((st.l2 - 1.0).abs() < 1e-6);
    assert!(st.r2 < 1e-6);
    assert!(st.pressed("l2") && !st.pressed("r2"));
    assert!(st.pressed("l1"), "buttons fall back to the standard map");
}

#[test]
fn mode1_swaps_the_y_axes() {
    let map = PadMap::for_driver(Driver::HidPlaystation);
    let st = map.read(&playstation_fixture(), 0.5);
    let axes = pilot_axes(&st, &ds4().stick_modes["mode1"], 0.08);
    // Left stick up is pitch forward in mode 1.
    assert!((axes[1] - 1.0).abs() < 0.01);
    assert!(axes[2].abs() < 0.01);
}

#[test]
fn radial_deadzone_zeroes_the_centre_and_keeps_the_rim() {
    assert_eq!(radial_deadzone((0.05, 0.05), 0.08), (0.0, 0.0));
    let (x, _) = radial_deadzone((1.0, 0.0), 0.08);
    assert!((x - 1.0).abs() < 1e-6);
    let (x, _) = radial_deadzone((0.54, 0.0), 0.08);
    assert!((x - 0.5).abs() < 1e-3, "rescaled past the deadzone: {x}");
}

#[test]
fn a_centred_dead_band_on_a_real_range_reads_zero() {
    // DS4 sticks rest a count or two off 128.
    let mut p = playstation_fixture();
    p.abs.insert(ABS_Y, ax(130));
    p.abs.insert(ABS_RX, ax(126));
    let st = PadMap::for_driver(Driver::HidPlaystation).read(&p, 0.5);
    assert_eq!(pilot_axes(&st, mode2(), 0.08), [0.0, 0.0, 0.0, 0.0]);
    assert!(is_centred(st.stick_l));
}
