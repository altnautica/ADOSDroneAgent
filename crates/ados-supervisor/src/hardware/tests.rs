use super::*;

#[test]
fn read_hex16_parses_sysfs_form() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("idVendor");
    std::fs::write(&f, "0bda\n").unwrap();
    assert_eq!(read_hex16(&f), Some(0x0BDA));
    assert_eq!(read_hex16(&dir.path().join("missing")), None);
}

#[test]
fn indexed_serial_node_requires_a_numeric_index() {
    assert!(is_indexed_serial_node("ttyACM0"));
    assert!(is_indexed_serial_node("ttyUSB12"));
    assert!(!is_indexed_serial_node("ttyACM"));
    assert!(!is_indexed_serial_node("ttyUSBx"));
    assert!(!is_indexed_serial_node("ttyprintk"));
    assert!(!is_indexed_serial_node("ttyS0"));
}

#[test]
fn usb_device_above_climbs_to_the_device_dir() {
    // The interface dir has no id files; the parent (the USB device) does.
    let dir = tempfile::tempdir().unwrap();
    let device = dir.path().join("usbdev");
    let iface = device.join("iface");
    std::fs::create_dir_all(&iface).unwrap();
    std::fs::write(device.join("idVendor"), "1a86\n").unwrap();
    std::fs::write(device.join("idProduct"), "7523\n").unwrap();
    std::fs::write(device.join("busnum"), "1\n").unwrap();
    std::fs::write(device.join("devnum"), "4\n").unwrap();
    let dev = usb_device_above(&iface).unwrap();
    assert_eq!((dev.vid, dev.pid), (0x1A86, 0x7523));
    assert_eq!(dev.identity(), "1-4 1a86:7523");
    // The same device re-enumerated gets a new device number.
    std::fs::write(device.join("devnum"), "5\n").unwrap();
    assert_ne!(usb_device_above(&iface).unwrap().identity(), dev.identity());
    // No USB ancestor anywhere -> None.
    let bare = dir.path().join("soc-uart");
    std::fs::create_dir_all(&bare).unwrap();
    assert_eq!(usb_device_above(&bare), None);
}

#[test]
fn boot_detect_accepts_every_adapter_the_generated_table_declares() {
    // The regression: this classification was a three-entry hardcode
    // (a81a / 8812 / 881a), so an RTL8812EU or either TP-Link variant read
    // as "no radio" and the supervisor never started the radio unit on
    // hardware the rest of the stack fully supports. Asserting against the
    // generated table — not a second copy of it — is what stops a future
    // table entry from silently going undetected at boot again.
    for (vid, pid, label) in ados_protocol::wfb_tables::WFB_COMPATIBLE {
        assert!(
            is_wfb_adapter_id(*vid, *pid),
            "{label} ({vid:#06x}:{pid:#06x}) is a declared WFB adapter but \
             boot detect does not recognise it"
        );
    }
}

#[test]
fn boot_detect_refuses_a_denied_management_radio_and_an_unknown_id() {
    // A management-WiFi chip that advertises monitor mode must never be
    // taken for an injection radio: starting the radio unit on it takes
    // down the box's own management link and still gives no video.
    for vid in ados_protocol::wfb_tables::DENY_VID {
        assert!(!is_wfb_adapter_id(*vid, 0x8812));
    }
    // A plain USB-serial bridge is not a radio.
    assert!(!is_wfb_adapter_id(0x1A86, 0x7523));
}
