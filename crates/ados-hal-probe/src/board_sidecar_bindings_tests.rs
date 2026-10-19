//! Board-level button and gauge bindings of the shipped Rockchip profiles.

use super::*;

fn profile(stem: &str) -> BoardProfile {
    load_profiles()
        .into_iter()
        .find(|p| p.stem == stem)
        .unwrap_or_else(|| panic!("{stem} must load"))
}

fn fingerprint(stem: &str) -> BoardFingerprint {
    from_profile(&profile(stem), "", &HostFacts::default())
}

#[test]
fn rock_boards_publish_header_buttons_clear_of_reserved_lines() {
    // ROCK 5B+: header pins 29/31/33/35.
    let fp = fingerprint("rock-5b-plus");
    assert_eq!(fp.buttons.map(|b| b.pins()), Some([63, 47, 103, 110]));
    // Debug console, I2C8, UART7, SPI0 panel lines and the PPS line.
    for line in [13, 14, 112, 113, 114, 115, 100, 41, 42, 43, 44, 45] {
        assert!(fp.reserved_gpio.contains(&line), "5B+ must reserve {line}");
    }
    // ROCK 5C / 5C Lite: header pins 29/31/33/35.
    let fp = fingerprint("rock-5c-lite");
    assert_eq!(fp.buttons.map(|b| b.pins()), Some([42, 41, 44, 128]));
    for line in [
        13, 14, 62, 63, 136, 137, 37, 32, 33, 34, 35, 36, 40, 45, 139,
    ] {
        assert!(fp.reserved_gpio.contains(&line), "5C must reserve {line}");
    }
    let v = serde_json::to_value(&fp).unwrap();
    assert_eq!(
        v["buttons"],
        serde_json::json!({"up": 42, "down": 41, "select": 44, "back": 128})
    );
}

#[test]
fn rock_boards_publish_an_encoder_on_free_header_lines() {
    // ROCK 5C / 5C Lite: header pins 36 (A), 38 (B), 15 (SW).
    let fp = fingerprint("rock-5c-lite");
    let enc = fp.encoder.expect("5C declares an encoder");
    assert_eq!(enc.pins(), [130, 133, 140]);
    assert_eq!(
        enc.pins().map(|p| (p / 32, p % 32)),
        [(4, 2), (4, 5), (4, 12)]
    );
    let v = serde_json::to_value(&fp).unwrap();
    assert_eq!(
        v["encoder"],
        serde_json::json!({"a": 130, "b": 133, "sw": 140})
    );
    // ROCK 5B+: header pins 36 (A), 38 (B), 40 (SW).
    let fp = fingerprint("rock-5b-plus");
    assert_eq!(fp.encoder.map(|e| e.pins()), Some([105, 106, 107]));
    for stem in ["rock-5c-lite", "rock-5b-plus"] {
        let fp = fingerprint(stem);
        let buttons = fp.buttons.expect("buttons").pins();
        for pin in fp.encoder.expect("encoder").pins() {
            assert!(
                !fp.reserved_gpio.contains(&pin),
                "{stem} encoder {pin} reserved"
            );
            assert!(!buttons.contains(&pin), "{stem} encoder {pin} is a button");
        }
    }
    // A board without the block publishes none.
    assert_eq!(fingerprint("cm5").encoder, None);
}

#[test]
fn an_encoder_on_a_reserved_button_or_repeated_line_rejects_the_profile() {
    let base = "name: Test\nreserved_gpio:\n  - {gpio: 13, header_pin: 8, use: console}\n\
                gnss: {transport: serial, pps_gpio: 100}\n\
                buttons: {up: 63, down: 47, select: 103, back: 110}\n";
    let ok = format!("{base}encoder: {{a: 105, b: 106, sw: 107}}\n");
    assert_eq!(
        parse_profile(&ok).unwrap().encoder.map(|e| e.pins()),
        Some([105, 106, 107])
    );
    let on_pps = format!("{base}encoder: {{a: 105, b: 100, sw: 107}}\n");
    assert_eq!(
        parse_profile(&on_pps).unwrap_err(),
        "encoder gpio 100 is a reserved line"
    );
    let on_button = format!("{base}encoder: {{a: 105, b: 106, sw: 110}}\n");
    assert_eq!(
        parse_profile(&on_button).unwrap_err(),
        "encoder gpio 110 is declared twice"
    );
    let repeated = format!("{base}encoder: {{a: 105, b: 105, sw: 107}}\n");
    assert!(parse_profile(&repeated).is_err());
    // An incomplete block does not parse.
    let partial = format!("{base}encoder: {{a: 105, b: 106}}\n");
    assert!(parse_profile(&partial).is_err());
}

#[test]
fn rock_boards_carry_no_default_battery_gauge() {
    // The shunt is a property of the rig's wiring, not of the board.
    for stem in ["rock-5b-plus", "rock-5c-lite"] {
        assert!(profile(stem).battery_gauge.is_none(), "{stem}");
        let v = serde_json::to_value(fingerprint(stem)).unwrap();
        assert!(v.get("battery_gauge").is_none(), "{stem}");
    }
}

#[test]
fn a_button_on_a_reserved_or_repeated_line_rejects_the_profile() {
    let base = "name: Test\nreserved_gpio:\n  - {gpio: 13, header_pin: 8, use: console}\n\
                gnss: {transport: serial, pps_gpio: 100}\n";
    let ok = format!("{base}buttons: {{up: 63, down: 47, select: 103, back: 110}}\n");
    assert!(parse_profile(&ok).is_ok());
    assert_eq!(
        parse_profile(&ok).unwrap().reserved_lines(),
        vec![13, 100],
        "PPS joins the reserved lines"
    );

    let on_console = format!("{base}buttons: {{up: 13, down: 47, select: 103, back: 110}}\n");
    assert_eq!(
        parse_profile(&on_console).unwrap_err(),
        "button gpio 13 is a reserved line"
    );
    let on_pps = format!("{base}buttons: {{up: 63, down: 100, select: 103, back: 110}}\n");
    assert!(parse_profile(&on_pps).is_err());
    let repeated = format!("{base}buttons: {{up: 63, down: 63, select: 103, back: 110}}\n");
    assert_eq!(
        parse_profile(&repeated).unwrap_err(),
        "button gpio 63 is declared twice"
    );
}

#[test]
fn a_board_gauge_without_its_shunt_values_does_not_parse() {
    for gauge in [
        "battery_gauge: {type: ina226, i2c_bus: 8, addr: 64}\n",
        "battery_gauge: {type: ina226, i2c_bus: 8, addr: 64, shunt_ohm: 0.001}\n",
        "battery_gauge: {type: ina226, i2c_bus: 8, addr: 64, max_current_a: 30}\n",
    ] {
        assert!(
            parse_profile(&format!("name: Test\n{gauge}")).is_err(),
            "{gauge}"
        );
    }
}
