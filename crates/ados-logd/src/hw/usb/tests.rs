use super::*;
use std::fs;

fn write_device(root: &Path, node: &str, files: &[(&str, &str)]) {
    let dir = root.join(format!("sys/bus/usb/devices/{node}"));
    fs::create_dir_all(&dir).unwrap();
    for (file, body) in files {
        fs::write(dir.join(file), body).unwrap();
    }
}

#[test]
fn reads_device_identity_and_speed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_device(
        root,
        "2-1",
        &[
            ("idVendor", "0BDA\n"),
            ("idProduct", "A81A\n"),
            ("busnum", "2\n"),
            ("devnum", "5\n"),
            ("speed", "480\n"),
        ],
    );
    let devs = read_usb_devices(root);
    assert_eq!(devs.len(), 1);
    let d = &devs[0];
    assert_eq!(d.bus, 2);
    assert_eq!(d.dev, 5);
    // vid/pid are lower-cased for stable matching.
    assert_eq!(d.vid, "0bda");
    assert_eq!(d.pid, "a81a");
    assert_eq!(d.speed_mbps, 480);
}

#[test]
fn fractional_low_speed_truncates_to_whole_mbps() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_device(
        root,
        "1-1",
        &[
            ("idVendor", "1d6b\n"),
            ("idProduct", "0002\n"),
            ("busnum", "1\n"),
            ("devnum", "1\n"),
            ("speed", "1.5\n"),
        ],
    );
    let devs = read_usb_devices(root);
    assert_eq!(devs.len(), 1);
    assert_eq!(devs[0].speed_mbps, 1);
}

#[test]
fn interfaces_and_bus_roots_without_ids_are_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // An interface node: has a speed-less subtree and no idVendor.
    write_device(root, "2-1:1.0", &[("bInterfaceClass", "ff\n")]);
    // A device missing speed is skipped too.
    write_device(
        root,
        "2-2",
        &[("idVendor", "0bda\n"), ("idProduct", "8153\n")],
    );
    assert!(read_usb_devices(root).is_empty());
}

#[test]
fn devices_are_sorted_by_bus_then_dev() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_device(
        root,
        "2-3",
        &[
            ("idVendor", "aaaa\n"),
            ("idProduct", "0001\n"),
            ("busnum", "2\n"),
            ("devnum", "9\n"),
            ("speed", "480\n"),
        ],
    );
    write_device(
        root,
        "1-1",
        &[
            ("idVendor", "bbbb\n"),
            ("idProduct", "0002\n"),
            ("busnum", "1\n"),
            ("devnum", "2\n"),
            ("speed", "5000\n"),
        ],
    );
    let devs = read_usb_devices(root);
    assert_eq!(devs.len(), 2);
    // bus 1 comes before bus 2.
    assert_eq!((devs[0].bus, devs[0].dev), (1, 2));
    assert_eq!((devs[1].bus, devs[1].dev), (2, 9));
}

#[test]
fn empty_root_yields_no_devices() {
    let dir = tempfile::tempdir().unwrap();
    assert!(read_usb_devices(dir.path()).is_empty());
}
