//! Serial-port probe.
//!
//! Enumerates the serial device nodes exposed under `/sys/class/tty`
//! (CDC-ACM, USB-serial bridges), resolves the backing USB id where the port
//! is a USB bridge, and scores each as a flight-controller candidate.
//!
//! **Not consulted at runtime.** The LIVE flight-controller port selection
//! does not go through this scoring: it is the MAVLink router's own discovery
//! path (`FcConnection::candidate_ports()` filtering
//! `tokio_serial::available_ports()`, then a per-port baud sweep). This module
//! is the probe-first HAL's serial *inventory* layer — part of the
//! `probe_all()` capability snapshot, which has no service consumer yet — and
//! is exercised only by its tests today. When it IS wired, callers pass the
//! pinned CRSF/ELRS device (`radio.crsf.device`) so the RC module's port
//! scores 0, mirroring the exclusion the router's live path already enforces.
//!
//! The scoring and the sysfs walk are split: [`score_fc`] is a pure function
//! tested on any host, and [`enumerate_from_root`] takes the `/sys/class/tty`
//! root as a parameter so a fake layout can be parsed in a test.

use std::path::Path;

use ados_protocol::hwcaps::{Probed, SerialPort, UsbId};

/// The sysfs class directory every tty node exposes a link under.
#[cfg(target_os = "linux")]
const TTY_CLASS_DIR: &str = "/sys/class/tty";

/// Tty node-name prefixes the probe considers (USB-attached serial only).
const TTY_PREFIXES: [&str; 2] = ["ttyACM", "ttyUSB"];

/// USB vendor ids commonly seen on flight controllers / their USB-serial
/// bridges. A match adds to the candidate score; it is a heuristic hint, not a
/// gate (an unknown vendor still scores from its node kind). Several of these
/// bridges (CP210x, CH340, Espressif) equally front an ExpressLRS RC module —
/// the vendor id CANNOT tell the two apart, which is why the pinned CRSF
/// device is excluded by an explicit parameter rather than by vendor.
mod fc_vendors {
    /// Generic / open-hardware USB vendor id (used by many DIY FCs).
    pub const GENERIC: u16 = 0x1209;
    /// Silicon Labs CP210x USB-UART bridge.
    pub const CP210X: u16 = 0x10C4;
    /// STMicroelectronics (native USB on STM32-based FCs).
    pub const STM: u16 = 0x0483;
    /// USB vendor id frequently used by Arduino-compatible / serial boards.
    pub const ARDUINO: u16 = 0x2341;
    /// FTDI USB-serial bridge (common on UART-only FCs).
    pub const FTDI: u16 = 0x0403;
    /// WCH CH340 USB-UART bridge (common on budget UART-bridged boards).
    pub const CH340: u16 = 0x1A86;
    /// Espressif native USB (ESP32-S3-class boards).
    pub const ESPRESSIF: u16 = 0x303A;
}

/// Probe the serial ports, scoring FC candidates. Unwired today — see the
/// module docs: the router's live discovery path owns FC port selection.
///
/// `crsf_pin` is the pinned CRSF/ELRS device (`radio.crsf.device`), if any.
/// The pinned port stays in the returned inventory (it exists, and hiding it
/// would misreport the hardware) but is forced to `fc_score` 0: it is never an
/// FC candidate, so it can never win the best-first sort. A score of 0 means
/// "not an FC candidate" — a caller picking the head of the list must treat 0
/// as no-candidate rather than a weak one.
///
/// Returns [`Probed::Present`] with the scored ports sorted best-first, or
/// [`Probed::Absent`] with
/// [`AbsenceReason::NodeMissing`](ados_protocol::hwcaps::AbsenceReason) when no
/// USB-serial tty exists. Non-Linux hosts return [`Probed::NotProbed`] (there is
/// no `/sys/class/tty` to read).
#[cfg(target_os = "linux")]
pub fn probe_serial_ports(crsf_pin: Option<&str>) -> Probed<Vec<SerialPort>> {
    use ados_protocol::hwcaps::{AbsenceReason, Evidence};

    let ports = enumerate_from_root(Path::new(TTY_CLASS_DIR), crsf_pin);
    if ports.is_empty() {
        return Probed::absent(AbsenceReason::NodeMissing);
    }
    Probed::present(ports, Evidence::SysfsPath(TTY_CLASS_DIR.to_string()))
}

/// On non-Linux hosts there is no `/sys/class/tty`, so the probe never ran.
#[cfg(not(target_os = "linux"))]
pub fn probe_serial_ports(_crsf_pin: Option<&str>) -> Probed<Vec<SerialPort>> {
    Probed::NotProbed
}

/// Walk a `/sys/class/tty`-shaped root, build a scored [`SerialPort`] for every
/// USB-serial node, and return them sorted by score descending (path ascending
/// to break ties so the order is stable).
///
/// A node named by `crsf_pin` (compared by node basename, so `/dev/ttyUSB0`
/// and `ttyUSB0` both match; the caller resolves any symlink spelling first)
/// stays in the inventory but scores 0 — the pinned RC module is a real serial
/// port, just never an FC candidate.
///
/// Factored out of [`probe_serial_ports`] so a fake layout can be fed in by a
/// test without touching the real `/sys`.
#[allow(dead_code)]
fn enumerate_from_root(tty_root: &Path, crsf_pin: Option<&str>) -> Vec<SerialPort> {
    let pin_node = crsf_pin
        .map(|p| p.trim().trim_start_matches("/dev/"))
        .unwrap_or("");
    let mut ports: Vec<SerialPort> = Vec::new();

    let Ok(rd) = std::fs::read_dir(tty_root) else {
        return ports;
    };
    for entry in rd.flatten() {
        let name = entry.file_name();
        let node = name.to_string_lossy();
        if !TTY_PREFIXES.iter().any(|p| has_indexed_prefix(&node, p)) {
            continue;
        }
        let usb = resolve_usb_id(&entry.path().join("device"));
        let fc_score = if !pin_node.is_empty() && node == pin_node {
            0
        } else {
            score_fc(&node, usb.as_ref())
        };
        ports.push(SerialPort {
            path: format!("/dev/{node}"),
            usb,
            fc_score,
        });
    }

    ports.sort_by(|a, b| {
        b.fc_score
            .cmp(&a.fc_score)
            .then_with(|| a.path.cmp(&b.path))
    });
    ports
}

/// True if `name` is `prefix` followed by one or more decimal digits
/// (`ttyACM0`, `ttyUSB12`), so we skip `tty`, `ttyprintk`, and similar.
#[allow(dead_code)]
fn has_indexed_prefix(name: &str, prefix: &str) -> bool {
    match name.strip_prefix(prefix) {
        Some(rest) => !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()),
        None => false,
    }
}

/// Resolve the `device` link of a tty node to the backing USB device id.
///
/// The `device` link points at the USB *interface* directory; `idVendor` /
/// `idProduct` live on the parent USB *device* directory, so we walk up parents
/// until a directory carries both files. Returns `None` for SoC UARTs (no USB
/// ancestor) or unreadable links.
#[allow(dead_code)]
fn resolve_usb_id(device_link: &Path) -> Option<UsbId> {
    use std::path::PathBuf;

    // Canonicalize follows the symlink into /sys/devices/...; fall back to the
    // raw path when canonicalization is unavailable (e.g. a fake test layout
    // that uses real subdirectories rather than symlinks).
    let start = std::fs::canonicalize(device_link).unwrap_or_else(|_| device_link.to_path_buf());
    let mut dir: Option<PathBuf> = Some(start);
    while let Some(d) = dir {
        if let (Some(vid), Some(pid)) = (
            read_hex16(&d.join("idVendor")),
            read_hex16(&d.join("idProduct")),
        ) {
            return Some(UsbId { vid, pid });
        }
        dir = d.parent().map(Path::to_path_buf);
        // Stop at the sysfs roots so we never climb out of /sys.
        if dir
            .as_deref()
            .is_some_and(|p| p == Path::new("/sys") || p == Path::new("/"))
        {
            break;
        }
    }
    None
}

/// Parse a sysfs hex16 file (`idVendor` / `idProduct` hold a 4-digit hex word).
#[allow(dead_code)]
fn read_hex16(p: &Path) -> Option<u16> {
    let s = std::fs::read_to_string(p).ok()?;
    u16::from_str_radix(s.trim(), 16).ok()
}

/// Score a serial node 0-100 as a flight-controller candidate.
///
/// Pure: depends only on the node name (CDC-ACM scores above raw USB-serial,
/// since native-USB FCs enumerate as ACM) and the backing USB vendor id (a
/// known FC / bridge vendor lifts the score). No I/O, so it is unit-tested
/// directly on any host.
#[allow(dead_code)]
fn score_fc(node: &str, usb: Option<&UsbId>) -> u8 {
    // Base score by node kind: a native-USB FC presents as CDC-ACM, while a
    // UART-bridge dongle (FTDI / CP210x) presents as ttyUSB — both plausible,
    // ACM more so.
    let mut score: u8 = if node.starts_with("ttyACM") {
        50
    } else if node.starts_with("ttyUSB") {
        30
    } else {
        0
    };

    // A recognized FC / bridge vendor is a strong hint on top of the node kind.
    if let Some(id) = usb {
        let known = matches!(
            id.vid,
            fc_vendors::GENERIC
                | fc_vendors::CP210X
                | fc_vendors::STM
                | fc_vendors::ARDUINO
                | fc_vendors::FTDI
                | fc_vendors::CH340
                | fc_vendors::ESPRESSIF
        );
        if known {
            score = score.saturating_add(40);
        } else {
            // Any USB backing at all is still better than an unidentified node.
            score = score.saturating_add(5);
        }
    }

    score.min(100)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acm_outscores_usb_for_same_vendor() {
        let stm = UsbId {
            vid: fc_vendors::STM,
            pid: 0x5740,
        };
        assert!(score_fc("ttyACM0", Some(&stm)) > score_fc("ttyUSB0", Some(&stm)));
    }

    #[test]
    fn known_vendor_outscores_unknown_on_same_node() {
        let known = UsbId {
            vid: fc_vendors::CP210X,
            pid: 0xEA60,
        };
        let unknown = UsbId {
            vid: 0xABCD,
            pid: 0x1234,
        };
        assert!(score_fc("ttyACM0", Some(&known)) > score_fc("ttyACM0", Some(&unknown)));
        assert!(score_fc("ttyACM0", Some(&unknown)) > score_fc("ttyACM0", None));
    }

    #[test]
    fn ch340_and_espressif_are_known_bridge_vendors() {
        // Budget UART-bridged boards ship on CH340; ESP32-S3-class boards
        // enumerate under the Espressif native-USB vendor id. Both take the
        // known-vendor bump, same as FTDI/CP210x.
        for vid in [fc_vendors::CH340, fc_vendors::ESPRESSIF] {
            let id = UsbId { vid, pid: 0x0001 };
            assert_eq!(score_fc("ttyUSB0", Some(&id)), 70);
        }
    }

    #[test]
    fn score_is_bounded_and_kind_ranked() {
        let generic = UsbId {
            vid: fc_vendors::GENERIC,
            pid: 0x5741,
        };
        // Highest plausible: ACM + known vendor.
        assert_eq!(score_fc("ttyACM0", Some(&generic)), 90);
        assert!(score_fc("ttyACM0", None) > score_fc("ttyUSB0", None));
        // A non-serial node never scores.
        assert_eq!(score_fc("ttyS0", None), 0);
    }

    #[test]
    fn indexed_prefix_rejects_bare_and_named_nodes() {
        assert!(has_indexed_prefix("ttyACM0", "ttyACM"));
        assert!(has_indexed_prefix("ttyUSB12", "ttyUSB"));
        assert!(!has_indexed_prefix("ttyACM", "ttyACM"));
        assert!(!has_indexed_prefix("ttyACMx", "ttyACM"));
        assert!(!has_indexed_prefix("ttyprintk", "ttyUSB"));
    }

    /// Build a fake `/sys/class/tty` layout with real subdirectories standing
    /// in for the symlinked `device` -> USB-device chain, then assert the walk
    /// finds every node, resolves the USB ids, scores them, and sorts best
    /// first.
    #[test]
    fn enumerate_parses_fake_sysfs_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let tty = tmp.path();

        // ttyACM0 backed by an STM native-USB FC (idVendor on the resolved
        // `device` dir, with an interface subdir below it).
        let acm_dev = tty.join("ttyACM0/device");
        std::fs::create_dir_all(acm_dev.join("iface")).unwrap();
        std::fs::write(acm_dev.join("idVendor"), "0483\n").unwrap();
        std::fs::write(acm_dev.join("idProduct"), "5740\n").unwrap();

        // ttyUSB0 backed by an unknown vendor.
        let usb_dev = tty.join("ttyUSB0/device");
        std::fs::create_dir_all(&usb_dev).unwrap();
        std::fs::write(usb_dev.join("idVendor"), "abcd\n").unwrap();
        std::fs::write(usb_dev.join("idProduct"), "0001\n").unwrap();

        // A SoC UART with no USB ancestor (no idVendor anywhere).
        std::fs::create_dir_all(tty.join("ttyACM1/device")).unwrap();

        // Noise that must be ignored.
        std::fs::create_dir_all(tty.join("tty")).unwrap();
        std::fs::create_dir_all(tty.join("ttyprintk")).unwrap();

        let ports = enumerate_from_root(tty, None);

        // Three indexed USB-serial nodes; tty / ttyprintk skipped.
        assert_eq!(ports.len(), 3, "expected 3 ports, got {ports:?}");

        // Best first: ttyACM0 with a known vendor outranks the rest.
        assert_eq!(ports[0].path, "/dev/ttyACM0");
        assert_eq!(
            ports[0].usb,
            Some(UsbId {
                vid: 0x0483,
                pid: 0x5740
            })
        );

        // The SoC UART resolved no USB id but is still enumerated.
        let acm1 = ports.iter().find(|p| p.path == "/dev/ttyACM1").unwrap();
        assert!(acm1.usb.is_none());

        // Scores are monotonically non-increasing (sorted best-first).
        for win in ports.windows(2) {
            assert!(win[0].fc_score >= win[1].fc_score);
        }
    }

    /// A pinned CRSF device stays in the inventory (it is a real serial port)
    /// but scores 0 and sorts last: it can never win the FC-candidate sort.
    #[test]
    fn pinned_crsf_device_scores_zero_but_stays_enumerated() {
        let tmp = tempfile::tempdir().unwrap();
        let tty = tmp.path();

        // ttyACM0: an STM native-USB FC.
        let acm_dev = tty.join("ttyACM0/device");
        std::fs::create_dir_all(&acm_dev).unwrap();
        std::fs::write(acm_dev.join("idVendor"), "0483\n").unwrap();
        std::fs::write(acm_dev.join("idProduct"), "5740\n").unwrap();

        // ttyUSB0: a CP2102 bridge fronting the pinned RC module. Without the
        // pin it would score 70 (ttyUSB + known bridge vendor).
        let usb_dev = tty.join("ttyUSB0/device");
        std::fs::create_dir_all(&usb_dev).unwrap();
        std::fs::write(usb_dev.join("idVendor"), "10c4\n").unwrap();
        std::fs::write(usb_dev.join("idProduct"), "ea60\n").unwrap();

        let ports = enumerate_from_root(tty, Some("/dev/ttyUSB0"));
        assert_eq!(ports.len(), 2);
        assert_eq!(ports[0].path, "/dev/ttyACM0");
        assert_eq!(ports[1].path, "/dev/ttyUSB0");
        assert_eq!(
            ports[1].fc_score, 0,
            "pinned port must never be a candidate"
        );
        // The USB id is still reported truthfully on the excluded port.
        assert_eq!(
            ports[1].usb,
            Some(UsbId {
                vid: 0x10C4,
                pid: 0xEA60
            })
        );

        // The bare-node-name pin spelling matches too.
        let ports = enumerate_from_root(tty, Some("ttyUSB0"));
        assert_eq!(ports[1].fc_score, 0);

        // A pin naming an absent node changes nothing.
        let ports = enumerate_from_root(tty, Some("/dev/ttyUSB9"));
        assert!(ports.iter().all(|p| p.fc_score > 0));
    }

    #[test]
    fn missing_tty_root_enumerates_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let nope = tmp.path().join("does-not-exist");
        assert!(enumerate_from_root(&nope, None).is_empty());
    }
}
