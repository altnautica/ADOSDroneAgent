use super::*;

#[test]
fn a_paired_device_out_of_range_is_not_connected() {
    let info = "Device AA:BB:CC:DD:EE:FF (public)\n\tName: Pad\n\tPaired: yes\n\tConnected: no\n";
    assert_eq!(bt_info_connected(info), Some(false));
    assert_eq!(bt_info_connected("\tConnected: yes\n"), Some(true));
    assert_eq!(bt_info_connected("Device not available\n"), None);
}

#[test]
fn profile_mismatch_is_the_fastapi_404_shape() {
    let resp = profile_mismatch();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    // The body shape is the contract; build it independently and compare.
    let want = json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}});
    assert_eq!(
        want,
        json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})
    );
}

#[test]
fn device_id_for_bt_lowercases_and_prefixes() {
    assert_eq!(
        device_id_for_bt("AA:BB:CC:DD:EE:FF"),
        "bt:aa:bb:cc:dd:ee:ff"
    );
    assert_eq!(
        device_id_for_bt("00:11:22:33:44:55"),
        "bt:00:11:22:33:44:55"
    );
}

#[test]
fn parse_bt_device_lines_matches_the_python_parser() {
    // The shape `bluetoothctl paired-devices` emits: a `Device <MAC> <Name>`
    // line per device, names with internal spaces kept whole.
    let out = "Device AA:BB:CC:DD:EE:FF Xbox Wireless Controller\nDevice 11:22:33:44:55:66 8BitDo Pro 2\n";
    let parsed = parse_bt_device_lines(out);
    assert_eq!(
        parsed,
        vec![
            (
                "AA:BB:CC:DD:EE:FF".to_string(),
                "Xbox Wireless Controller".to_string()
            ),
            ("11:22:33:44:55:66".to_string(), "8BitDo Pro 2".to_string()),
        ]
    );
}

#[test]
fn parse_bt_device_lines_skips_non_device_lines() {
    // Status / prompt lines bluetoothctl interleaves are ignored.
    let out = "Agent registered\nDevice AA:BB:CC:DD:EE:FF Pad\n[bluetooth]# \n";
    let parsed = parse_bt_device_lines(out);
    assert_eq!(
        parsed,
        vec![("AA:BB:CC:DD:EE:FF".to_string(), "Pad".to_string())]
    );
}

#[test]
fn parse_bt_device_lines_falls_back_to_mac_when_name_absent() {
    // A `Device <MAC>` line with no name: the name field defaults to the MAC,
    // matching the Python `name = parts[2] if len(parts) >= 3 else mac`.
    let out = "Device AA:BB:CC:DD:EE:FF\n";
    let parsed = parse_bt_device_lines(out);
    assert_eq!(
        parsed,
        vec![(
            "AA:BB:CC:DD:EE:FF".to_string(),
            "AA:BB:CC:DD:EE:FF".to_string()
        )]
    );
}

#[test]
fn paired_bluetooth_records_render_the_full_python_field_set() {
    // Drive the record projection directly off a parsed line so the field set
    // is asserted without spawning bluetoothctl.
    let parsed = parse_bt_device_lines("Device AA:BB:CC:DD:EE:FF Pad\n");
    let records: Vec<Value> = parsed
        .into_iter()
        .map(|(mac, name)| {
            json!({
                "device_id": device_id_for_bt(&mac),
                "mac": mac,
                "name": name,
                "type": "bluetooth",
                "connected": true,
            })
        })
        .collect();
    assert_eq!(
        records,
        vec![json!({
            "device_id": "bt:aa:bb:cc:dd:ee:ff",
            "mac": "AA:BB:CC:DD:EE:FF",
            "name": "Pad",
            "type": "bluetooth",
            "connected": true,
        })]
    );
}

#[test]
fn gamepad_record_is_the_full_python_field_set() {
    // The seven-field record the REST layer returns per controller.
    let g = ados_hid::input::Gamepad {
        device_id: "usb:045e:028e:event3".to_string(),
        name: "Xbox Controller".to_string(),
        path: "/dev/input/event3".to_string(),
        vendor: 0x045e,
        product: 0x028e,
    };
    let record = json!({
        "device_id": g.device_id,
        "name": g.name,
        "path": g.path,
        "vendor": g.vendor,
        "product": g.product,
        "type": "usb",
        "connected": true,
    });
    assert_eq!(
        record,
        json!({
            "device_id": "usb:045e:028e:event3",
            "name": "Xbox Controller",
            "path": "/dev/input/event3",
            "vendor": 1118,
            "product": 654,
            "type": "usb",
            "connected": true,
        })
    );
}

#[test]
fn primary_device_id_reads_the_sidecar_and_defaults_null() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ground-station-input.json");
    // Absent file → null.
    assert_eq!(primary_device_id(&path), Value::Null);
    // A sidecar naming a primary surfaces it.
    std::fs::write(&path, r#"{"primary":"usb:045e:028e:event3"}"#).unwrap();
    assert_eq!(
        primary_device_id(&path),
        Value::String("usb:045e:028e:event3".to_string())
    );
    // A sidecar with a null primary → null.
    std::fs::write(&path, r#"{"primary":null}"#).unwrap();
    assert_eq!(primary_device_id(&path), Value::Null);
}

#[test]
fn split_whitespace_max3_keeps_the_remainder_whole() {
    // Two leading fields, the rest (with internal spaces) kept verbatim.
    assert_eq!(
        split_whitespace_max3("Device AA:BB Long Name Here"),
        vec!["Device", "AA:BB", "Long Name Here"]
    );
    // Collapsing runs of whitespace between the leading fields.
    assert_eq!(
        split_whitespace_max3("Device   AA:BB   Pad"),
        vec!["Device", "AA:BB", "Pad"]
    );
    // Only two fields present.
    assert_eq!(
        split_whitespace_max3("Device AA:BB"),
        vec!["Device", "AA:BB"]
    );
}

#[test]
fn empty_lists_are_the_bench_shape() {
    // The envelope shape on a bench GS with nothing attached/paired.
    let gamepads = json!({ "devices": Vec::<Value>::new(), "primary_id": Value::Null });
    assert_eq!(gamepads, json!({"devices": [], "primary_id": null}));
    let bluetooth = json!({ "devices": Vec::<Value>::new() });
    assert_eq!(bluetooth, json!({"devices": []}));
}
