//! Boot-time hardware detection: camera + WFB radio adapter.
//!
//! Mirrors the Python supervisor's detect pass. All reads are filesystem /
//! subprocess probes, so the module compiles on any host (the probes simply
//! find nothing off a real SBC).

use std::path::Path;

use tokio::process::Command;

/// True if a video node exists or a CSI camera is present.
pub async fn has_camera() -> bool {
    if video_node_present() {
        return true;
    }
    csi_camera_present().await
}

/// `/dev/video[0-9]+` present.
pub fn video_node_present() -> bool {
    dev_nodes_present(&["video"])
}

async fn csi_camera_present() -> bool {
    match Command::new("rpicam-hello")
        .arg("--list-cameras")
        .output()
        .await
    {
        Ok(out) => String::from_utf8_lossy(&out.stdout).contains("Available cameras"),
        Err(_) => false,
    }
}

/// True if an RTL8812-family WFB adapter is on the USB bus (boot detect set:
/// the same VID/PID triple the Python `_check_wfb_adapter` uses).
pub fn has_wfb_adapter() -> bool {
    const WFB_IDS: [(u16, u16); 3] = [(0x0BDA, 0xA81A), (0x0BDA, 0x8812), (0x0BDA, 0x881A)];
    enumerate_usb_ids().iter().any(|id| WFB_IDS.contains(id))
}

/// Read `(idVendor, idProduct)` for every device under `/sys/bus/usb/devices`.
pub fn enumerate_usb_ids() -> Vec<(u16, u16)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/sys/bus/usb/devices") else {
        return out;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if let (Some(v), Some(pid)) = (
            read_hex16(&p.join("idVendor")),
            read_hex16(&p.join("idProduct")),
        ) {
            out.push((v, pid));
        }
    }
    out
}

fn read_hex16(p: &Path) -> Option<u16> {
    let s = std::fs::read_to_string(p).ok()?;
    u16::from_str_radix(s.trim(), 16).ok()
}

/// True if any `/dev/<prefix>[0-9]+` node exists for one of `prefixes`.
pub fn dev_nodes_present(prefixes: &[&str]) -> bool {
    let Ok(rd) = std::fs::read_dir("/dev") else {
        return false;
    };
    for entry in rd.flatten() {
        let name = entry.file_name();
        let n = name.to_string_lossy();
        for pre in prefixes {
            if let Some(rest) = n.strip_prefix(pre) {
                if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_hex16_parses_sysfs_form() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("idVendor");
        std::fs::write(&f, "0bda\n").unwrap();
        assert_eq!(read_hex16(&f), Some(0x0BDA));
        assert_eq!(read_hex16(&dir.path().join("missing")), None);
    }
}
