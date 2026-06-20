//! Serial-device enumeration for the FC-source picker.
//!
//! `GET /api/mavlink/ports` lists the serial devices a flight controller could
//! be attached to, so the setup webapp + the GCS can offer a dropdown instead of
//! making the operator type a `/dev/tty*` path. Each entry is `{path,
//! description}`: the `path` is the device node to write into `mavlink.source =
//! serial` + `mavlink.serial_port`, the `description` is a human-friendly label
//! (the `by-id` link name when present, else the bare device name).
//!
//! Enumeration is a filesystem scan, never a probe: it lists the kernel's
//! `/dev/serial/by-id/*` USB-serial symlinks (whose names carry the vendor /
//! product string) and the bare `/dev/tty{ACM,USB,AMA,S}*` nodes, de-duplicated
//! by their resolved device path. It opens nothing and sends nothing, so it is
//! side-effect-free and safe to poll. An absent `/dev` (a non-Linux host) yields
//! an empty list rather than an error.

use std::collections::BTreeMap;
use std::path::Path;

use axum::Json;
use serde_json::{json, Value};

/// `GET /api/mavlink/ports` → `{ports: [{path, description}, ...]}`.
///
/// Lists candidate FC serial devices from a filesystem scan. Guaranteed 200:
/// an unreadable `/dev` degrades to an empty list (the same shape a host with no
/// serial devices returns).
pub async fn list_ports() -> Json<Value> {
    Json(json!({ "ports": enumerate_ports() }))
}

/// The device-name prefixes a flight controller commonly enumerates as: USB CDC
/// ACM (`ttyACM*`), USB serial (`ttyUSB*`), the Pi/SoC PL011 UARTs (`ttyAMA*`),
/// and the legacy 16550 ports (`ttyS*`). Anything else (pty, console, etc.) is
/// not an FC candidate and is skipped.
const TTY_PREFIXES: [&str; 4] = ["ttyACM", "ttyUSB", "ttyAMA", "ttyS"];

/// Build the de-duplicated port list, keyed by the resolved device path so a
/// `by-id` symlink and its bare `/dev/tty*` target collapse to one entry (the
/// `by-id` description, which is more descriptive, wins). Sorted by path for a
/// stable order. Pure over the two scan roots so a test can drive it with a
/// tempdir; the route uses the real `/dev` paths.
fn enumerate_ports() -> Vec<Value> {
    enumerate_ports_in(Path::new("/dev/serial/by-id"), Path::new("/dev"))
}

/// The path-injectable core of [`enumerate_ports`].
fn enumerate_ports_in(by_id_dir: &Path, dev_dir: &Path) -> Vec<Value> {
    // path → description, ordered (BTreeMap) so the output is deterministic.
    let mut found: BTreeMap<String, String> = BTreeMap::new();

    // The descriptive `by-id` symlinks first: their link name carries the
    // vendor/product string, so it is the better description. Resolve each to its
    // device-node target and key on that so the later bare-node scan does not add
    // a duplicate.
    if let Ok(entries) = std::fs::read_dir(by_id_dir) {
        for entry in entries.flatten() {
            let link = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let resolved = std::fs::canonicalize(&link)
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| link.to_string_lossy().into_owned());
            // Description = the readable by-id link name (underscores in vendor
            // strings are kept verbatim, as the kernel writes them).
            found.entry(resolved).or_insert(name);
        }
    }

    // The bare device nodes, for ports the kernel did not create a by-id link for
    // (the SoC UARTs `ttyAMA*` / legacy `ttyS*` never get one). Keep only the
    // FC-candidate prefixes. The description is the bare device path.
    if let Ok(entries) = std::fs::read_dir(dev_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !TTY_PREFIXES.iter().any(|p| name.starts_with(p)) {
                continue;
            }
            let path = entry.path();
            let resolved = std::fs::canonicalize(&path)
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned());
            found
                .entry(resolved.clone())
                .or_insert_with(|| resolved.clone());
        }
    }

    found
        .into_iter()
        .map(|(path, description)| json!({ "path": path, "description": description }))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    #[test]
    fn empty_dirs_yield_an_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let ports = enumerate_ports_in(
            &dir.path().join("by-id-absent"),
            &dir.path().join("dev-absent"),
        );
        assert!(ports.is_empty());
    }

    #[test]
    fn bare_tty_nodes_are_listed_and_non_candidates_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let dev = dir.path().join("dev");
        fs::create_dir_all(&dev).unwrap();
        // Candidates.
        fs::write(dev.join("ttyACM0"), b"").unwrap();
        fs::write(dev.join("ttyUSB0"), b"").unwrap();
        fs::write(dev.join("ttyAMA0"), b"").unwrap();
        // Non-candidates.
        fs::write(dev.join("tty1"), b"").unwrap();
        fs::write(dev.join("random"), b"").unwrap();

        let ports = enumerate_ports_in(&dir.path().join("by-id-absent"), &dev);
        let paths: Vec<&str> = ports
            .iter()
            .map(|p| p.get("path").unwrap().as_str().unwrap())
            .collect();
        assert!(paths.iter().any(|p| p.ends_with("/ttyACM0")));
        assert!(paths.iter().any(|p| p.ends_with("/ttyUSB0")));
        assert!(paths.iter().any(|p| p.ends_with("/ttyAMA0")));
        assert!(!paths.iter().any(|p| p.ends_with("/tty1")));
        assert!(!paths.iter().any(|p| p.ends_with("/random")));
    }

    #[test]
    fn a_by_id_link_supplies_the_description_and_dedups_the_bare_node() {
        let dir = tempfile::tempdir().unwrap();
        let dev = dir.path().join("dev");
        let by_id = dir.path().join("by-id");
        fs::create_dir_all(&dev).unwrap();
        fs::create_dir_all(&by_id).unwrap();

        let node = dev.join("ttyACM0");
        fs::write(&node, b"").unwrap();
        // The descriptive by-id symlink → the same device node.
        symlink(&node, by_id.join("usb-ArduPilot_Pixhawk-if00")).unwrap();

        let ports = enumerate_ports_in(&by_id, &dev);
        // One entry (the symlink + bare node collapse on the resolved path), and
        // the description is the readable by-id name.
        assert_eq!(ports.len(), 1, "by-id link must dedup the bare node");
        let desc = ports[0].get("description").unwrap().as_str().unwrap();
        assert_eq!(desc, "usb-ArduPilot_Pixhawk-if00");
        let path = ports[0].get("path").unwrap().as_str().unwrap();
        assert!(path.ends_with("/ttyACM0"));
    }
}
