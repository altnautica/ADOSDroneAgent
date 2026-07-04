//! Best-effort hardware detection for the onboarding wizard.
//!
//! The wizard runs BEFORE the agent is installed, so it cannot use the agent's
//! own HAL. It takes one cheap snapshot of the system ([`SysProbe`]) — `lsusb`,
//! the `/dev` listing, the board model, the display/i2c/GPU state — and every
//! catalog category classifies itself from that snapshot (see
//! [`crate::wizard::catalog`]). All probes are best-effort and never fail: an
//! absent tool or an empty result just reads as "not detected". The parsing is
//! pure so it is unit-tested without any hardware.

use crate::exec;

/// What the pre-wizard hardware sweep found on this box. The camera is kept in
/// the reduced shape because the component checklist pre-seeds its default from
/// it; every other category is classified on demand from the full [`Self::sys`]
/// snapshot by the profile-filtered hardware catalog.
#[derive(Debug, Clone, Default)]
pub struct HardwareProbe {
    /// First `/dev/video*` node, when a camera is present.
    pub camera: Option<String>,
    /// The full system snapshot the hardware catalog classifies from.
    pub sys: SysProbe,
}

/// One cheap snapshot of the host the whole catalog is classified from, so the
/// wizard shells out once rather than per-category. Cloneable + pure to classify,
/// so the catalog detection is unit-tested by constructing a snapshot by hand.
#[derive(Debug, Clone, Default)]
pub struct SysProbe {
    /// Raw `lsusb` output (empty when the tool is absent).
    pub lsusb: String,
    /// Entry names under `/dev` (no path prefix).
    pub dev_names: Vec<String>,
    /// The board / machine model, from the device tree or `/proc/cpuinfo`.
    pub board_model: Option<String>,
    /// A monitor is connected on some DRM connector (HDMI/DP).
    pub hdmi_connected: bool,
    /// I2C 7-bit addresses seen on any bus (e.g. `0x3c` for a common OLED).
    pub i2c_addrs: Vec<u8>,
    /// A render GPU is present (`/dev/dri/card*` or an lspci VGA/3D line).
    pub gpu: bool,
}

/// USB ids of the long-range radio family, for the presence hint. The radio
/// stack (`ados-radio`) owns the authoritative classification; this short list
/// only pre-seeds a checklist default the operator can still toggle.
const RADIO_USB_IDS: &[(u16, u16)] = &[
    (0x0BDA, 0x8812),
    (0x0BDA, 0x881A),
    (0x0BDA, 0x881B),
    (0x0BDA, 0x881C),
    (0x0BDA, 0xA81A),
    (0x0BDA, 0xB812),
    (0x2357, 0x0120),
    (0x2357, 0x0101),
];

/// USB vendor ids of common u-blox GNSS receivers (the presence hint only).
const GPS_USB_IDS: &[(u16, u16)] = &[
    (0x1546, 0x01A6), // u-blox 6
    (0x1546, 0x01A7), // u-blox 7
    (0x1546, 0x01A8), // u-blox 8 / M8
    (0x1546, 0x01A9), // u-blox 9
];

/// USB vendor ids of common LTE / 4G modems (vendor match: any product).
const MODEM_VENDOR_IDS: &[u16] = &[
    0x2C7C, // Quectel
    0x1E0E, // SimCom
    0x12D1, // Huawei
    0x1199, // Sierra Wireless
    0x2CB7, // Fibocom
    0x05C6, // Qualcomm (many rebadged modems)
];

/// Run the full sweep once, before the first screen.
pub fn probe() -> HardwareProbe {
    let sys = snapshot();
    HardwareProbe {
        camera: pick_lowest_video(&sys.dev_names).map(|n| format!("/dev/{n}")),
        sys,
    }
}

/// Take the one system snapshot the catalog classifies from.
pub fn snapshot() -> SysProbe {
    SysProbe {
        lsusb: exec::run("lsusb", &[]).stdout,
        dev_names: read_dev_names(),
        board_model: read_board_model(),
        hdmi_connected: read_hdmi_connected(),
        i2c_addrs: read_i2c_addrs(),
        gpu: read_gpu_present(),
    }
}

/// The board / machine model from the device tree (embedded boards) or the first
/// `/proc/cpuinfo` "model name" (x86 desktops), trimmed of a trailing NUL.
fn read_board_model() -> Option<String> {
    for p in [
        "/proc/device-tree/model",
        "/sys/firmware/devicetree/base/model",
    ] {
        if let Ok(s) = std::fs::read_to_string(p) {
            let m = s.trim_end_matches('\0').trim();
            if !m.is_empty() {
                return Some(m.to_string());
            }
        }
    }
    if let Ok(cpuinfo) = std::fs::read_to_string("/proc/cpuinfo") {
        for line in cpuinfo.lines() {
            if let Some((k, v)) = line.split_once(':') {
                if matches!(k.trim(), "model name" | "Model") {
                    let v = v.trim();
                    if !v.is_empty() {
                        return Some(v.to_string());
                    }
                }
            }
        }
    }
    None
}

/// True when any DRM connector reports `connected` (a monitor is plugged in).
fn read_hdmi_connected() -> bool {
    let Ok(read) = std::fs::read_dir("/sys/class/drm") else {
        return false;
    };
    for e in read.flatten() {
        let status = e.path().join("status");
        if let Ok(s) = std::fs::read_to_string(&status) {
            if s.trim() == "connected" {
                return true;
            }
        }
    }
    false
}

/// The set of 7-bit I2C addresses present across the first few buses, via
/// `i2cdetect -y <bus>`. Empty when `i2cdetect` or `/dev/i2c-*` is absent.
fn read_i2c_addrs() -> Vec<u8> {
    let mut addrs = Vec::new();
    for bus in 0..8u8 {
        if !std::path::Path::new(&format!("/dev/i2c-{bus}")).exists() {
            continue;
        }
        let out = exec::run("i2cdetect", &["-y", &bus.to_string()]);
        if out.success() {
            parse_i2cdetect(&out.stdout, &mut addrs);
        }
    }
    addrs.sort_unstable();
    addrs.dedup();
    addrs
}

/// True when a render GPU is present: a `/dev/dri/card*` node, or an lspci line
/// naming a VGA / 3D / Display controller.
fn read_gpu_present() -> bool {
    if let Ok(read) = std::fs::read_dir("/dev/dri") {
        if read
            .flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with("card"))
        {
            return true;
        }
    }
    let out = exec::run("lspci", &[]);
    out.success()
        && out.stdout.lines().any(|l| {
            let l = l.to_ascii_lowercase();
            l.contains("vga compatible controller")
                || l.contains("3d controller")
                || l.contains("display controller")
        })
}

/// List the entries under `/dev` (names only). Empty on a host without `/dev`.
fn read_dev_names() -> Vec<String> {
    match std::fs::read_dir("/dev") {
        Ok(read) => read
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .collect(),
        Err(_) => Vec::new(),
    }
}

// ── pure classifiers (unit-tested without hardware) ─────────────────────────

/// Pick the lowest-numbered `videoN` from a list of `/dev` entry names (pure).
pub fn pick_lowest_video(names: &[String]) -> Option<String> {
    let mut best: Option<(u32, String)> = None;
    for name in names {
        if let Some(digits) = name.strip_prefix("video") {
            if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
                if let Ok(n) = digits.parse::<u32>() {
                    if best.as_ref().map(|(bn, _)| n < *bn).unwrap_or(true) {
                        best = Some((n, name.clone()));
                    }
                }
            }
        }
    }
    best.map(|(_, name)| name)
}

/// Pick the first serial-port name matching one of `prefixes`, honoring the
/// prefix order then lexical order within a prefix (pure).
pub fn pick_first_serial(names: &[String], prefixes: &[&str]) -> Option<String> {
    for prefix in prefixes {
        let mut matches: Vec<&String> = names
            .iter()
            .filter(|n| n.starts_with(prefix) && n.len() > prefix.len())
            .collect();
        matches.sort();
        if let Some(first) = matches.first() {
            return Some((*first).clone());
        }
    }
    None
}

/// The first joystick / gamepad node (`/dev/input/jsN`), by name (pure). The
/// `input/` prefix is stripped by `read_dev_names` so match on `js*`.
pub fn pick_joystick(names: &[String]) -> Option<String> {
    let mut matches: Vec<&String> = names
        .iter()
        .filter(|n| {
            n.strip_prefix("js")
                .map(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()))
                .unwrap_or(false)
        })
        .collect();
    matches.sort();
    matches.first().map(|s| (*s).clone())
}

/// The number of long-range radio adapters `lsusb` lists (pure). Two or more is
/// the mesh-capable ground-station configuration.
pub fn radio_count(output: &str) -> usize {
    output
        .lines()
        .filter(|l| {
            parse_lsusb_id(l)
                .map(|(v, p)| RADIO_USB_IDS.iter().any(|(rv, rp)| *rv == v && *rp == p))
                .unwrap_or(false)
        })
        .count()
}

/// True when `lsusb` output contains a long-range radio USB id (pure).
pub fn lsusb_has_radio(output: &str) -> bool {
    radio_count(output) >= 1
}

/// True when `lsusb` output contains a known u-blox GNSS id (pure).
pub fn lsusb_has_gps(output: &str) -> bool {
    output.lines().any(|l| {
        parse_lsusb_id(l)
            .map(|(v, p)| GPS_USB_IDS.iter().any(|(gv, gp)| *gv == v && *gp == p))
            .unwrap_or(false)
    })
}

/// True when `lsusb` output contains a known LTE-modem vendor id (pure).
pub fn lsusb_has_modem(output: &str) -> bool {
    output.lines().any(|l| {
        parse_lsusb_id(l)
            .map(|(v, _)| MODEM_VENDOR_IDS.contains(&v))
            .unwrap_or(false)
    })
}

/// Extract the `vvvv:pppp` USB id from one `lsusb` line (pure). Returns the
/// (vendor, product) pair, or `None` when the line has no id token.
fn parse_lsusb_id(line: &str) -> Option<(u16, u16)> {
    let mut tokens = line.split_whitespace();
    while let Some(tok) = tokens.next() {
        if tok == "ID" {
            let id = tokens.next()?;
            let (v, p) = id.split_once(':')?;
            let vid = u16::from_str_radix(v.trim(), 16).ok()?;
            let pid = u16::from_str_radix(p.trim(), 16).ok()?;
            return Some((vid, pid));
        }
    }
    None
}

/// Parse the addresses out of one `i2cdetect -y N` grid into `out` (pure). Each
/// data cell is a two-hex-digit address or `--`/`UU`; the header row + the row
/// labels (`00:`) are skipped.
pub fn parse_i2cdetect(output: &str, out: &mut Vec<u8>) {
    for line in output.lines() {
        let Some((label, cells)) = line.split_once(':') else {
            continue;
        };
        // Row labels are two hex digits (the high nibble * 0x10). The header
        // ("     0  1  2 ...") has no colon and is skipped by the split above.
        if u8::from_str_radix(label.trim(), 16).is_err() {
            continue;
        }
        for tok in cells.split_whitespace() {
            if let Ok(addr) = u8::from_str_radix(tok, 16) {
                out.push(addr);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn lowest_video_node_wins_and_non_numeric_is_ignored() {
        let names = owned(&["video2", "video0", "video10", "videoX", "null", "video"]);
        assert_eq!(pick_lowest_video(&names), Some("video0".to_string()));
        assert_eq!(pick_lowest_video(&owned(&["null", "tty"])), None);
    }

    #[test]
    fn first_serial_honors_prefix_then_lexical_order() {
        let names = owned(&["ttyUSB0", "ttyACM1", "ttyACM0", "ttyAMA0"]);
        assert_eq!(
            pick_first_serial(&names, &["ttyACM", "ttyAMA", "ttyUSB"]),
            Some("ttyACM0".to_string())
        );
        assert_eq!(
            pick_first_serial(&owned(&["ttyUSB0"]), &["ttyACM", "ttyAMA", "ttyUSB"]),
            Some("ttyUSB0".to_string())
        );
        assert_eq!(pick_first_serial(&owned(&["random"]), &["ttyACM"]), None);
    }

    #[test]
    fn joystick_node_detected_by_name() {
        assert_eq!(
            pick_joystick(&owned(&["js1", "js0", "mouse0"])),
            Some("js0".to_string())
        );
        assert_eq!(pick_joystick(&owned(&["event0", "mice"])), None);
    }

    #[test]
    fn lsusb_id_parses_and_matches_the_radio_family() {
        assert!(lsusb_has_radio(
            "Bus 001 Device 004: ID 0bda:8812 Realtek Semiconductor Corp."
        ));
        assert!(lsusb_has_radio("Bus 002 Device 003: ID 0bda:b812 Realtek 8812EU"));
        assert!(!lsusb_has_radio(
            "Bus 001 Device 002: ID 0bda:0411 Realtek USB hub"
        ));
        assert!(!lsusb_has_radio(
            "Bus 001 Device 005: ID a69c:8801 AIC Semiconductor"
        ));
        assert!(!lsusb_has_radio(""));
        assert!(!lsusb_has_radio("no id here"));
    }

    #[test]
    fn two_radios_are_mesh_capable() {
        let two = "Bus 001 Device 004: ID 0bda:8812 Realtek\n\
                   Bus 001 Device 005: ID 0bda:8812 Realtek";
        assert_eq!(radio_count(two), 2);
        assert_eq!(radio_count("Bus 001 Device 004: ID 0bda:8812 Realtek"), 1);
        assert_eq!(radio_count(""), 0);
    }

    #[test]
    fn gps_and_modem_vendor_matching() {
        assert!(lsusb_has_gps(
            "Bus 001 Device 006: ID 1546:01a8 U-Blox AG"
        ));
        assert!(!lsusb_has_gps("Bus 001 Device 006: ID 1546:9999 U-Blox other"));
        assert!(lsusb_has_modem(
            "Bus 002 Device 003: ID 2c7c:0125 Quectel EC25"
        ));
        assert!(lsusb_has_modem("Bus 002 Device 004: ID 12d1:1506 Huawei"));
        assert!(!lsusb_has_modem("Bus 001 Device 002: ID 0bda:0411 hub"));
    }

    #[test]
    fn i2cdetect_grid_addresses_parse() {
        // A slimmed i2cdetect grid with a device at 0x3c (a common OLED).
        let grid = "     0  1  2  3  4  5  6  7  8  9  a  b  c  d  e  f\n\
                    00:                         -- -- -- -- -- -- -- --\n\
                    30: -- -- -- -- -- -- -- -- -- -- -- -- 3c -- -- --\n\
                    70: -- -- -- -- -- -- -- --";
        let mut addrs = Vec::new();
        parse_i2cdetect(grid, &mut addrs);
        assert!(addrs.contains(&0x3c), "expected 0x3c, got {addrs:?}");
        // The header row ("0 1 2 ...") must not be misread as addresses.
        assert!(!addrs.contains(&0x00));
    }
}
