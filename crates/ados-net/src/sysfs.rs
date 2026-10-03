//! Shared sysfs interface discovery used by the daemon and the failover policy.
//!
//! Predictable wired-iface names vary across BSPs (`eth0`, `end1`, `enp*`,
//! `enx*`), so the wired uplink is resolved by scanning `/sys/class/net` rather
//! than assuming `eth0`. The router prefers the resolved wired iface, so this
//! lives in the library where both the daemon and the priority policy reach it.

/// Resolve the physical ethernet iface name. Scan `/sys/class/net` for the
/// first non-virtual wired device that exposes a carrier file; fall back to
/// `eth0` when nothing matches (the manager then reads a missing carrier as
/// "down", which is correct on a board with no NIC).
pub fn detect_ethernet_iface() -> String {
    let read = match std::fs::read_dir("/sys/class/net") {
        Ok(rd) => rd,
        Err(_) => return "eth0".to_string(),
    };
    let mut candidates: Vec<String> = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let wired = name.starts_with("eth") || name.starts_with("en");
        if !wired {
            continue;
        }
        // Skip virtual ifaces (no device symlink under the iface dir).
        let dev_link = entry.path().join("device");
        let carrier = entry.path().join("carrier");
        if dev_link.exists() && carrier.exists() {
            candidates.push(name);
        }
    }
    candidates.sort();
    candidates
        .into_iter()
        .next()
        .unwrap_or_else(|| "eth0".to_string())
}
