//! Best-effort hardware detection for the onboarding wizard.
//!
//! The wizard shows the operator what it found (a camera, a flight controller,
//! a long-range radio) and pre-seeds the component checklist from it, so every
//! screen confirms a sensible default rather than asking from a blank slate.
//!
//! All three probes are best-effort and never fail: an absent tool or an empty
//! result just reads as "not detected". The parsing is pure so it is unit-tested
//! without any hardware.

use crate::exec;

/// What the pre-wizard hardware sweep found on this box.
#[derive(Debug, Clone, Default)]
pub struct HardwareProbe {
    /// First `/dev/video*` node, when a camera is present.
    pub camera: Option<String>,
    /// First flight-controller serial port, when one is present.
    pub fc: Option<String>,
    /// A long-range (WFB) USB radio is attached.
    pub radio: bool,
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

/// Run the full sweep once, before the first screen.
pub fn probe() -> HardwareProbe {
    HardwareProbe {
        camera: detect_camera(),
        fc: detect_fc(),
        radio: detect_radio(),
    }
}

/// The lowest-numbered `/dev/video*` capture node, or `None`.
fn detect_camera() -> Option<String> {
    let names = read_dev_names();
    pick_lowest_video(&names).map(|n| format!("/dev/{n}"))
}

/// The first flight-controller serial port in the canonical glob order
/// (`ttyACM* ttyAMA* ttyUSB*`), or `None`. Matches the seed detection the
/// config step uses for the mavlink serial port.
fn detect_fc() -> Option<String> {
    let names = read_dev_names();
    pick_first_serial(&names, &["ttyACM", "ttyAMA", "ttyUSB"]).map(|n| format!("/dev/{n}"))
}

/// True when `lsusb` lists a long-range radio USB id.
fn detect_radio() -> bool {
    let res = exec::run("lsusb", &[]);
    res.success() && lsusb_has_radio(&res.stdout)
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

/// True when `lsusb` output contains a long-range radio USB id (pure). Each
/// `lsusb` line carries `... ID vvvv:pppp ...`; we match that pair against the
/// radio family list.
pub fn lsusb_has_radio(output: &str) -> bool {
    for line in output.lines() {
        if let Some((vid, pid)) = parse_lsusb_id(line) {
            if RADIO_USB_IDS.iter().any(|(v, p)| *v == vid && *p == pid) {
                return true;
            }
        }
    }
    false
}

/// Extract the `vvvv:pppp` USB id from one `lsusb` line (pure). Returns the
/// (vendor, product) pair, or `None` when the line has no id token.
fn parse_lsusb_id(line: &str) -> Option<(u16, u16)> {
    let mut tokens = line.split_whitespace();
    // Find the "ID" token; the next token is "vvvv:pppp".
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
        // No numeric video node → none.
        assert_eq!(pick_lowest_video(&owned(&["null", "tty"])), None);
    }

    #[test]
    fn first_serial_honors_prefix_then_lexical_order() {
        let names = owned(&["ttyUSB0", "ttyACM1", "ttyACM0", "ttyAMA0"]);
        // ACM before AMA before USB; within ACM, ACM0 before ACM1.
        assert_eq!(
            pick_first_serial(&names, &["ttyACM", "ttyAMA", "ttyUSB"]),
            Some("ttyACM0".to_string())
        );
        // Only a USB adapter present → it is chosen once the earlier prefixes miss.
        assert_eq!(
            pick_first_serial(&owned(&["ttyUSB0"]), &["ttyACM", "ttyAMA", "ttyUSB"]),
            Some("ttyUSB0".to_string())
        );
        assert_eq!(pick_first_serial(&owned(&["random"]), &["ttyACM"]), None);
    }

    #[test]
    fn lsusb_id_parses_and_matches_the_radio_family() {
        let radio = "Bus 001 Device 004: ID 0bda:8812 Realtek Semiconductor Corp.";
        assert!(lsusb_has_radio(radio));
        let radio_eu = "Bus 002 Device 003: ID 0bda:b812 Realtek 8812EU";
        assert!(lsusb_has_radio(radio_eu));
        // A non-radio Realtek device does not match.
        let hub = "Bus 001 Device 002: ID 0bda:0411 Realtek USB hub";
        assert!(!lsusb_has_radio(hub));
        // The onboard management-WiFi vendor is not a long-range radio.
        let aic = "Bus 001 Device 005: ID a69c:8801 AIC Semiconductor";
        assert!(!lsusb_has_radio(aic));
        // Empty / malformed input is safe.
        assert!(!lsusb_has_radio(""));
        assert!(!lsusb_has_radio("no id here"));
    }

    #[test]
    fn lsusb_id_token_extraction() {
        assert_eq!(
            parse_lsusb_id("Bus 001 Device 004: ID 0bda:8812 Realtek"),
            Some((0x0BDA, 0x8812))
        );
        assert_eq!(parse_lsusb_id("Bus 001 Device 004:"), None);
    }
}
