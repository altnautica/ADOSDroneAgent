//! Profile-aware hardware-check engine.
//!
//! Mirrors `src/ados/setup/hardware_check.py` from the Python full agent.
//! For the lite agent this is a focused subset — board fingerprint, FC
//! serial path presence, camera enumeration via /dev/video*, and Wi-Fi
//! adapter detection via lsusb. Optional components (OLED, buttons, HDMI,
//! joystick, mesh dongle) are surfaced with state "warning" or "unknown"
//! rather than "ok"/"missing" because the lite profile doesn't ship those
//! services and the operator does not need them to fly.
//!
//! Each probe is best-effort and tolerates missing /proc, missing
//! /sys, missing tools (lsusb). Failure mode is "unknown" + a `fix_hint`
//! pointing at what the operator needs to install or check.

use std::path::Path;
use std::process::Command;

use crate::models::{HardwareCheckItem, HardwareCheckStatus};

const BOARDS_DIR_DEFAULT: &str = "/etc/ados/hal/boards";

fn now_iso() -> String {
    chrono::Local::now()
        .with_timezone(&chrono::FixedOffset::east_opt(5 * 3600 + 30 * 60).unwrap())
        .format("%Y-%m-%dT%H:%M:%S%:z")
        .to_string()
}

/// Run the full hardware sweep for the active profile and return a
/// canonical HardwareCheckStatus. Cheap (few hundred ms) — runs every /proc
/// read + a single lsusb invocation. Safe to call from any handler.
pub fn run_hardware_check(profile: &str, ground_role: &str) -> HardwareCheckStatus {
    let mut items: Vec<HardwareCheckItem> = Vec::new();
    items.push(check_board());
    items.push(check_fc());
    if profile == "drone" {
        items.push(check_camera());
        items.push(check_wifi());
    } else if profile == "ground_station" {
        items.push(check_wifi_wfb_adapter());
        items.push(check_hdmi());
    }
    HardwareCheckStatus {
        profile: profile.to_string(),
        ground_role: ground_role.to_string(),
        items,
        last_run: now_iso(),
    }
}

// ---------------------------------------------------------------------------
// Board fingerprint
// ---------------------------------------------------------------------------

fn check_board() -> HardwareCheckItem {
    let model = read_device_tree_model().or_else(read_cpuinfo_hardware);
    match model {
        Some(m) if !m.is_empty() => {
            let ram_kb = read_meminfo_total_kb().unwrap_or(0);
            let ram_mb = ram_kb / 1024;
            let detail = if ram_mb > 0 {
                format!("{m} ({ram_mb} MB RAM)")
            } else {
                m.clone()
            };
            let item = HardwareCheckItem::new("board", "Companion compute")
                .required(true)
                .ok(detail);
            // If we have a HAL boards directory, surface a warning when the
            // running model doesn't match any catalogued board. The lite
            // agent does not maintain a separate registry; it reads the
            // same YAMLs the Python agent uses.
            if let Some(matched) = match_board_id(&m) {
                let detail = format!("{m} ({ram_mb} MB RAM, board: {matched})");
                return HardwareCheckItem::new("board", "Companion compute")
                    .required(true)
                    .ok(detail);
            }
            item
        }
        _ => HardwareCheckItem::new("board", "Companion compute")
            .required(true)
            .warning(
                "Could not fingerprint board",
                "Check that /proc/device-tree/model is readable.",
            ),
    }
}

fn read_device_tree_model() -> Option<String> {
    let raw = std::fs::read_to_string("/proc/device-tree/model").ok()?;
    Some(raw.trim_end_matches('\0').trim().to_string())
}

fn read_cpuinfo_hardware() -> Option<String> {
    let raw = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("Hardware") {
            if let Some(value) = rest.splitn(2, ':').nth(1) {
                return Some(value.trim().to_string());
            }
        }
    }
    None
}

fn read_meminfo_total_kb() -> Option<u64> {
    let raw = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            return rest
                .trim()
                .split_whitespace()
                .next()
                .and_then(|n| n.parse().ok());
        }
    }
    None
}

/// Static board metadata for the heartbeat enrichment. Populated once at
/// agent startup from the same `/proc` probes the wizard uses, plus a
/// best-effort match against the HAL board registry to recover the
/// human-readable name + SoC string.
#[derive(Debug, Clone, Default)]
pub struct BoardMetadata {
    pub board_name: Option<String>,
    pub soc: Option<String>,
    pub arch: Option<String>,
    pub ram_mb: Option<u32>,
}

/// Detect the running board. Returns whatever we could read; missing
/// fields stay `None`. Cheap (~couple ms) — three `/proc` reads + one
/// HAL directory walk.
pub fn detect_board_metadata() -> BoardMetadata {
    let mut meta = BoardMetadata::default();
    let model = read_device_tree_model().or_else(read_cpuinfo_hardware);
    let ram_kb = read_meminfo_total_kb().unwrap_or(0);
    if ram_kb > 0 {
        meta.ram_mb = Some((ram_kb / 1024) as u32);
    }
    meta.arch = uname_machine();
    if let Some(m) = model.as_deref() {
        if let Some((display_name, soc)) = lookup_board_yaml(m) {
            meta.board_name = Some(display_name);
            meta.soc = soc;
        } else {
            // No HAL match — surface the raw model string so the GCS
            // still has something to render.
            meta.board_name = Some(m.to_string());
        }
    }
    meta
}

fn uname_machine() -> Option<String> {
    // Cheaper than spawning `uname -m`. Falls back to the compile-time
    // arch when both fail.
    if let Ok(raw) = std::fs::read_to_string("/proc/sys/kernel/arch") {
        let trimmed = raw.trim().to_string();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }
    Some(std::env::consts::ARCH.to_string())
}

fn lookup_board_yaml(model: &str) -> Option<(String, Option<String>)> {
    let dir = std::env::var_os("ADOS_HAL_BOARDS_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(BOARDS_DIR_DEFAULT));
    if !dir.is_dir() {
        return None;
    }
    let model_lower = model.to_lowercase();
    for entry in std::fs::read_dir(&dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("yaml") {
            continue;
        }
        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let doc: serde_yaml::Value = match serde_yaml::from_str(&raw) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let patterns = doc
            .get("model_patterns")
            .and_then(|v| v.as_sequence())
            .cloned()
            .unwrap_or_default();
        for p in patterns {
            if let Some(p) = p.as_str() {
                if model_lower.contains(&p.to_lowercase()) {
                    let name = doc
                        .get("name")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| model.to_string());
                    let soc = doc
                        .get("soc")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    return Some((name, soc));
                }
            }
        }
    }
    None
}

fn match_board_id(model: &str) -> Option<String> {
    let dir = std::env::var_os("ADOS_HAL_BOARDS_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(BOARDS_DIR_DEFAULT));
    if !dir.is_dir() {
        return None;
    }
    let model_lower = model.to_lowercase();
    for entry in std::fs::read_dir(&dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("yaml") {
            continue;
        }
        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let doc: serde_yaml::Value = match serde_yaml::from_str(&raw) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let patterns = doc
            .get("model_patterns")
            .and_then(|v| v.as_sequence())
            .cloned()
            .unwrap_or_default();
        for p in patterns {
            if let Some(p) = p.as_str() {
                if model_lower.contains(&p.to_lowercase()) {
                    let id = doc
                        .get("board")
                        .and_then(|b| b.get("id"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| {
                            path.file_stem()
                                .and_then(|s| s.to_str())
                                .unwrap_or("unknown")
                                .to_string()
                        });
                    return Some(id);
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Flight controller serial
// ---------------------------------------------------------------------------

fn check_fc() -> HardwareCheckItem {
    // The lite agent tracks the configured serial path in agent.yaml. For
    // the wizard's hardware-check we just probe the common candidates so
    // the operator sees presence even before MAVLink lock. A proper FC
    // heartbeat probe lives in the agent's runtime status surface; the
    // wizard inherits it via /api/v1/setup/status (mavlink.connected).
    let candidates = ["/dev/ttyS0", "/dev/ttyAMA0", "/dev/ttyUSB0", "/dev/ttyACM0"];
    let present: Vec<&str> = candidates
        .iter()
        .copied()
        .filter(|p| Path::new(p).exists())
        .collect();
    if present.is_empty() {
        return HardwareCheckItem::new("fc", "Flight controller (serial)")
            .required(true)
            .missing(
                "No flight controller serial device detected.",
                "Connect FC USB cable to the companion and re-check, or wire UART.",
            );
    }
    HardwareCheckItem::new("fc", "Flight controller (serial)")
        .required(true)
        .ok(format!(
            "Serial device(s) present: {}. MAVLink heartbeat is checked separately.",
            present.join(", ")
        ))
}

// ---------------------------------------------------------------------------
// Camera (V4L2 + sysfs)
// ---------------------------------------------------------------------------

fn check_camera() -> HardwareCheckItem {
    let v4l_dir = Path::new("/sys/class/video4linux");
    let mut device_names: Vec<String> = Vec::new();
    if v4l_dir.is_dir() {
        if let Ok(read) = std::fs::read_dir(v4l_dir) {
            for entry in read.flatten() {
                let name = entry.file_name();
                let name_s = name.to_string_lossy();
                if name_s.starts_with("video") {
                    device_names.push(name_s.to_string());
                }
            }
        }
    }
    // Fallback: enumerate /dev/video* even when sysfs isn't mounted.
    if device_names.is_empty() {
        if let Ok(read) = std::fs::read_dir("/dev") {
            for entry in read.flatten() {
                let name = entry.file_name();
                let name_s = name.to_string_lossy();
                if name_s.starts_with("video")
                    && name_s.chars().skip(5).all(|c| c.is_ascii_digit())
                {
                    device_names.push(name_s.to_string());
                }
            }
        }
    }
    if device_names.is_empty() {
        return HardwareCheckItem::new("camera", "Camera")
            .required(true)
            .missing(
                "No CSI or USB camera detected (no /dev/video* devices).",
                "Plug in a USB UVC camera or attach a MIPI CSI module.",
            );
    }
    device_names.sort();
    HardwareCheckItem::new("camera", "Camera").required(true).ok(format!(
        "{} device(s): /dev/{}",
        device_names.len(),
        device_names.join(", /dev/")
    ))
}

// ---------------------------------------------------------------------------
// Wi-Fi adapter (any) — for cloud relay uplink
// ---------------------------------------------------------------------------

fn check_wifi() -> HardwareCheckItem {
    let interfaces = list_wireless_interfaces();
    if interfaces.is_empty() {
        return HardwareCheckItem::new("wifi", "Wi-Fi uplink")
            .required(false)
            .warning(
                "No wireless interface detected.",
                "Optional. Plug in USB Wi-Fi or use Ethernet for cloud relay uplink.",
            );
    }
    HardwareCheckItem::new("wifi", "Wi-Fi uplink").required(false).ok(format!(
        "Wireless interface(s): {}",
        interfaces.join(", ")
    ))
}

fn list_wireless_interfaces() -> Vec<String> {
    let dir = Path::new("/sys/class/net");
    let mut out = Vec::new();
    if !dir.is_dir() {
        return out;
    }
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return out,
    };
    for entry in read.flatten() {
        let path = entry.path();
        if path.join("wireless").exists() {
            if let Some(name) = entry.file_name().to_str() {
                out.push(name.to_string());
            }
        }
    }
    out.sort();
    out
}

// ---------------------------------------------------------------------------
// WFB-ng RTL8812 USB radio (ground-station profile)
// ---------------------------------------------------------------------------

fn check_wifi_wfb_adapter() -> HardwareCheckItem {
    // Vendor:product IDs of supported WFB adapters. Mirrors Rule 31 — we
    // describe the chip the agent talks to without naming partners or
    // upstream firmware projects.
    let known_ids: &[(&str, &str)] = &[
        ("0bda", "8812"), // RTL8812AU
        ("0bda", "881a"), // RTL8812EU
        ("0bda", "8814"), // RTL8814AU
    ];
    match lsusb_devices() {
        Some(devs) => {
            let mut matches = Vec::new();
            for d in &devs {
                if known_ids
                    .iter()
                    .any(|(v, p)| d.vendor_id.eq_ignore_ascii_case(v) && d.product_id.starts_with(p))
                {
                    matches.push(format!("{}:{}", d.vendor_id, d.product_id));
                }
            }
            if matches.is_empty() {
                HardwareCheckItem::new("radio_wfb", "WFB radio adapter")
                    .required(true)
                    .missing(
                        "No RTL8812-class adapter detected on USB.",
                        "Plug in a supported USB Wi-Fi adapter for WFB-ng.",
                    )
            } else {
                HardwareCheckItem::new("radio_wfb", "WFB radio adapter")
                    .required(true)
                    .ok(format!("{} adapter(s): {}", matches.len(), matches.join(", ")))
            }
        }
        None => HardwareCheckItem::new("radio_wfb", "WFB radio adapter")
            .required(true)
            .unknown("lsusb not available; cannot probe USB radios."),
    }
}

#[derive(Debug)]
struct UsbDevice {
    vendor_id: String,
    product_id: String,
    #[allow(dead_code)]
    description: String,
}

fn lsusb_devices() -> Option<Vec<UsbDevice>> {
    let out = Command::new("lsusb").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut devs = Vec::new();
    for line in text.lines() {
        // lsusb output: "Bus 001 Device 002: ID 0bda:8812 Realtek..."
        if let Some(idx) = line.find("ID ") {
            let rest = &line[idx + 3..];
            let (id, desc) = match rest.split_once(' ') {
                Some(x) => x,
                None => (rest, ""),
            };
            if let Some((v, p)) = id.split_once(':') {
                if v.len() == 4 && p.len() == 4 {
                    devs.push(UsbDevice {
                        vendor_id: v.to_string(),
                        product_id: p.to_string(),
                        description: desc.trim().to_string(),
                    });
                }
            }
        }
    }
    Some(devs)
}

// ---------------------------------------------------------------------------
// HDMI output (best-effort sysfs check)
// ---------------------------------------------------------------------------

fn check_hdmi() -> HardwareCheckItem {
    let drm = Path::new("/sys/class/drm");
    if !drm.is_dir() {
        return HardwareCheckItem::new("hdmi", "HDMI output")
            .unknown("DRM sysfs not available on this platform.");
    }
    let mut connected = Vec::new();
    let mut seen_any = false;
    if let Ok(read) = std::fs::read_dir(drm) {
        for entry in read.flatten() {
            let name = entry.file_name();
            let name_s = name.to_string_lossy().to_uppercase();
            if !name_s.contains("HDMI") {
                continue;
            }
            seen_any = true;
            let status_path = entry.path().join("status");
            if let Ok(value) = std::fs::read_to_string(&status_path) {
                if value.trim().eq_ignore_ascii_case("connected") {
                    connected.push(entry.file_name().to_string_lossy().to_string());
                }
            }
        }
    }
    if !connected.is_empty() {
        HardwareCheckItem::new("hdmi", "HDMI output")
            .ok(format!("Connected on {}", connected.join(", ")))
    } else if seen_any {
        HardwareCheckItem::new("hdmi", "HDMI output").warning(
            "HDMI port present but no display connected.",
            "Plug in an HDMI display if running headed.",
        )
    } else {
        HardwareCheckItem::new("hdmi", "HDMI output")
            .unknown("No HDMI connectors enumerated.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_board_id_handles_missing_dir() {
        let prev = std::env::var_os("ADOS_HAL_BOARDS_DIR");
        std::env::set_var("ADOS_HAL_BOARDS_DIR", "/nonexistent/__test__");
        assert!(match_board_id("Luckfox Pico Zero").is_none());
        match prev {
            Some(v) => std::env::set_var("ADOS_HAL_BOARDS_DIR", v),
            None => std::env::remove_var("ADOS_HAL_BOARDS_DIR"),
        }
    }

    #[test]
    fn match_board_id_finds_luckfox_in_synthetic_registry() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("rv1106-g3.yaml"),
            "name: Luckfox Pico Zero\nboard:\n  id: rv1106-g3\nmodel_patterns:\n  - rv1106-g3\n  - Luckfox Pico Zero\n",
        )
        .unwrap();
        let prev = std::env::var_os("ADOS_HAL_BOARDS_DIR");
        std::env::set_var("ADOS_HAL_BOARDS_DIR", dir.path());
        let id = match_board_id("Luckfox Pico Zero").unwrap();
        assert_eq!(id, "rv1106-g3");
        match prev {
            Some(v) => std::env::set_var("ADOS_HAL_BOARDS_DIR", v),
            None => std::env::remove_var("ADOS_HAL_BOARDS_DIR"),
        }
    }

    #[test]
    fn match_board_id_returns_none_for_unknown_model() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("rv1106-g3.yaml"),
            "name: Luckfox Pico Zero\nboard:\n  id: rv1106-g3\nmodel_patterns:\n  - rv1106-g3\n",
        )
        .unwrap();
        let prev = std::env::var_os("ADOS_HAL_BOARDS_DIR");
        std::env::set_var("ADOS_HAL_BOARDS_DIR", dir.path());
        assert!(match_board_id("Some Other Board").is_none());
        match prev {
            Some(v) => std::env::set_var("ADOS_HAL_BOARDS_DIR", v),
            None => std::env::remove_var("ADOS_HAL_BOARDS_DIR"),
        }
    }

    #[test]
    fn run_hardware_check_drone_profile_emits_required_items() {
        let status = run_hardware_check("drone", "");
        assert_eq!(status.profile, "drone");
        let ids: Vec<&str> = status.items.iter().map(|i| i.id.as_str()).collect();
        assert!(ids.contains(&"board"));
        assert!(ids.contains(&"fc"));
        assert!(ids.contains(&"camera"));
        assert!(ids.contains(&"wifi"));
        // last_run should be a non-empty ISO timestamp.
        assert!(!status.last_run.is_empty());
    }

    #[test]
    fn run_hardware_check_ground_station_emits_radio_and_hdmi() {
        let status = run_hardware_check("ground_station", "direct");
        assert_eq!(status.profile, "ground_station");
        assert_eq!(status.ground_role, "direct");
        let ids: Vec<&str> = status.items.iter().map(|i| i.id.as_str()).collect();
        assert!(ids.contains(&"radio_wfb"));
        assert!(ids.contains(&"hdmi"));
        // No camera item on ground-station; that's a drone-side check.
        assert!(!ids.contains(&"camera"));
    }

    #[test]
    fn item_builder_chain_sets_state_and_detail() {
        let item = HardwareCheckItem::new("test", "Test")
            .required(true)
            .ok("looks good");
        assert!(item.required);
        assert_eq!(item.state, "ok");
        assert_eq!(item.detail, "looks good");
    }

    #[test]
    fn item_missing_carries_fix_hint() {
        let item = HardwareCheckItem::new("test", "Test")
            .missing("nope", "plug it in");
        assert_eq!(item.state, "missing");
        assert_eq!(item.fix_hint, "plug it in");
    }
}
