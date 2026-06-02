//! USB enumeration-speed reader.
//!
//! Each device under `/sys/bus/usb/devices/*` that is a full device (rather than
//! an interface) exposes a `speed` file in Mbps (`1.5`, `12`, `480`, `5000`,
//! `10000`) and the `idVendor` / `idProduct` / `busnum` / `devnum` identity
//! files. Recording the negotiated speed over time is what surfaces the failure
//! where an adapter enumerates at 12 Mbps on a slow controller instead of the
//! 480 Mbps it should have negotiated, long after the fact.
//!
//! Only entries that carry both a `speed` and the id files are emitted; bus
//! roots and interface nodes (which expose neither a `speed` nor an `idVendor`)
//! are skipped.

use std::path::Path;

use super::reader::{list_dir, read_trimmed, read_u32, under};

/// One USB device's identity and negotiated link speed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbDevice {
    /// Bus number.
    pub bus: u32,
    /// Device address on the bus.
    pub dev: u32,
    /// Vendor id, lower-case hex string (e.g. `0bda`).
    pub vid: String,
    /// Product id, lower-case hex string (e.g. `a81a`).
    pub pid: String,
    /// Negotiated link speed in Mbps (fractional speeds round down: `1.5`->`1`).
    pub speed_mbps: u32,
}

/// Read every USB device under `/sys/bus/usb/devices` that exposes a `speed` and
/// an `idVendor`/`idProduct`. Returns a list sorted by `(bus, dev)` so the order
/// is stable across ticks for diffing.
pub fn read_usb_devices(root: &Path) -> Vec<UsbDevice> {
    let base = under(root, "/sys/bus/usb/devices");
    let mut out = Vec::new();
    for entry in list_dir(&base) {
        let ddir = base.join(&entry);
        // A full device carries idVendor/idProduct; interfaces and the bus root
        // do not, so absence here cleanly skips a non-device node.
        let (Some(vid), Some(pid)) = (
            read_trimmed(&ddir.join("idVendor")),
            read_trimmed(&ddir.join("idProduct")),
        ) else {
            continue;
        };
        let Some(speed_mbps) = read_speed_mbps(&ddir) else {
            continue;
        };
        out.push(UsbDevice {
            bus: read_u32(&ddir.join("busnum")).unwrap_or(0),
            dev: read_u32(&ddir.join("devnum")).unwrap_or(0),
            vid: vid.to_ascii_lowercase(),
            pid: pid.to_ascii_lowercase(),
            speed_mbps,
        });
    }
    out.sort_by_key(|d| (d.bus, d.dev));
    out
}

/// Read and parse the `speed` file (Mbps, possibly fractional like `1.5`).
/// Truncates to whole Mbps so the recorded value is the standard tier and never
/// a float artifact.
fn read_speed_mbps(ddir: &Path) -> Option<u32> {
    let raw = read_trimmed(&ddir.join("speed"))?;
    // `speed` can be `1.5`, `12`, `480`, `5000`. Parse as float, truncate.
    raw.parse::<f64>().ok().map(|v| v as u32)
}

#[cfg(test)]
mod tests {
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
}
