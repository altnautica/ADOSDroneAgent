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
mod tests;
