//! USB device-id probe.
//!
//! Enumerates `(idVendor, idProduct)` for every device under
//! `/sys/bus/usb/devices` (the same sysfs walk the supervisor's boot detect
//! uses) and returns them as [`UsbId`]s.

use ados_protocol::hwcaps::{Probed, UsbId};

/// The sysfs directory every USB device exposes a node under.
#[cfg(target_os = "linux")]
const USB_DEVICES_DIR: &str = "/sys/bus/usb/devices";

/// Probe the USB device ids on the bus.
///
/// Reads `idVendor` + `idProduct` (hex strings) for every entry under
/// `/sys/bus/usb/devices`. A present-but-empty bus still counts as
/// [`Probed::Present`] (an empty `Vec`): the sysfs tree existed and we read it,
/// so "no USB devices" is a genuine answer, not "we could not look". The
/// directory being unreadable (no USB sysfs on this host) is [`Probed::Absent`]
/// with [`AbsenceReason::NodeMissing`].
#[cfg(target_os = "linux")]
pub fn probe_usb_ids() -> Probed<Vec<UsbId>> {
    use ados_protocol::hwcaps::{AbsenceReason, Evidence};

    let Ok(rd) = std::fs::read_dir(USB_DEVICES_DIR) else {
        return Probed::absent(AbsenceReason::NodeMissing);
    };
    let mut ids = Vec::new();
    for entry in rd.flatten() {
        let p = entry.path();
        let vid = std::fs::read_to_string(p.join("idVendor")).ok();
        let pid = std::fs::read_to_string(p.join("idProduct")).ok();
        if let (Some(vid), Some(pid)) = (vid, pid) {
            if let (Some(vid), Some(pid)) = (parse_hex16(&vid), parse_hex16(&pid)) {
                ids.push(UsbId { vid, pid });
            }
        }
    }
    Probed::present(ids, Evidence::SysfsPath(USB_DEVICES_DIR.to_string()))
}

/// On non-Linux hosts there is no `/sys/bus/usb`, so the probe never ran.
#[cfg(not(target_os = "linux"))]
pub fn probe_usb_ids() -> Probed<Vec<UsbId>> {
    Probed::NotProbed
}

/// Parse a sysfs `idVendor` / `idProduct` value (a hex string, optionally with
/// surrounding whitespace) into a `u16`. Returns `None` on a non-hex or
/// out-of-range value.
#[allow(dead_code)]
fn parse_hex16(s: &str) -> Option<u16> {
    u16::from_str_radix(s.trim(), 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_typical_sysfs_ids() {
        // The RTL8812-family WFB adapter ids the supervisor's boot detect uses.
        assert_eq!(parse_hex16("0bda"), Some(0x0BDA));
        assert_eq!(parse_hex16("a81a"), Some(0xA81A));
        assert_eq!(parse_hex16("8812"), Some(0x8812));
    }

    #[test]
    fn tolerates_trailing_newline_and_whitespace() {
        // sysfs reads come back with a trailing newline.
        assert_eq!(parse_hex16("0bda\n"), Some(0x0BDA));
        assert_eq!(parse_hex16("  1d6b  "), Some(0x1D6B));
        assert_eq!(parse_hex16("\t0002\n"), Some(0x0002));
    }

    #[test]
    fn parses_full_range_bounds() {
        assert_eq!(parse_hex16("0000"), Some(0));
        assert_eq!(parse_hex16("ffff"), Some(0xFFFF));
        assert_eq!(parse_hex16("FFFF"), Some(0xFFFF));
    }

    #[test]
    fn rejects_non_hex_and_overflow() {
        assert_eq!(parse_hex16(""), None);
        assert_eq!(parse_hex16("zzzz"), None);
        assert_eq!(parse_hex16("0x0bda"), None);
        // wider than a u16 overflows rather than truncating.
        assert_eq!(parse_hex16("10000"), None);
    }
}
