//! The MAC-pin engine: sysfs enumeration, the per-adapter decision, and the
//! `systemd-networkd` `.link` provisioning. The pure parts (`render_link_file`,
//! `parse_match_block`, `classify_adapter`) are unit-tested without I/O; the
//! sysfs / `udevadm` / subprocess parts are Linux-gated.
//!
//! The provisioning path only ever writes a `.link` file (effective on the next
//! boot) — it never changes a live interface's address, so it cannot drop the
//! operator's management link. Re-tagging the live interface is a separate,
//! caller-gated action ([`apply_live`]).

use std::collections::HashMap;
use std::path::Path;

use crate::{
    derive_pinned_mac, is_known_stable_efuse, is_quirk_randomizer, AdapterSource, LearnerRecord,
    MacAddr, MacPinsState, UsbId,
};

/// Directory `systemd-networkd` reads `.link` drop-ins from.
pub const NETWORKD_DIR: &str = "/etc/systemd/network";
/// Where the per-adapter verdicts + learner memory are persisted.
pub const STATE_PATH: &str = "/etc/ados/mac-pins.state";
/// Machine-id sources, in preference order.
pub const MACHINE_ID_PATHS: [&str; 2] = ["/etc/machine-id", "/var/lib/dbus/machine-id"];
/// The learner flags a randomizer after the MAC has changed on this many boots.
pub const LEARN_THRESHOLD: u32 = 2;

/// A network adapter discovered under `/sys/class/net`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetAdapter {
    pub name: String,
    /// `None` for non-USB adapters (platform NICs, virtual interfaces).
    pub usb_id: Option<UsbId>,
    /// USB topology path (e.g. `5-1.3`), port-stable across reboots. Empty when
    /// `usb_id` is `None`.
    pub usb_path: String,
    /// Current hardware address, if readable.
    pub mac: Option<MacAddr>,
}

/// Runtime knobs from `network.mac_pin` config.
#[derive(Debug, Clone, Default)]
pub struct ReconcileConfig {
    pub enabled: bool,
    pub apply_live_allowed: bool,
    /// Operator overrides keyed by `vvvv:pppp` or interface name -> explicit MAC.
    pub overrides: HashMap<String, String>,
}

/// The decision for one adapter (pure, no I/O).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Stable efuse MAC — leave it alone.
    Stable,
    /// Pinning disabled by config.
    Disabled,
    /// A randomizer we cannot pin right now.
    Deferred(String),
    /// Write a `.link` pinning `mac`.
    Pin { mac: MacAddr, source: AdapterSource },
    /// The learner suspects randomization; surface for the operator to confirm.
    Candidate { proposed: Option<MacAddr> },
    /// Still gathering cross-boot evidence; no verdict yet.
    Observe,
}

// ── Pure helpers ────────────────────────────────────────────────────────────

/// The `.link` filename for an interface: a `10-` prefix so it sorts before the
/// stock board files (`50-...`) and wins.
pub fn link_file_name(iface: &str) -> String {
    format!("10-ados-mac-{iface}.link")
}

/// Render the full `.link` body (pure). `match_block` is the body of the
/// `[Match]` section (without the `[Match]` header) — mirroring the stock board
/// file's match so this file claims the same adapter. `NamePolicy=kernel`
/// preserves the kernel interface name so a `wpa_supplicant@<iface>` binding
/// keeps working; an explicit `MACAddress=` sets the address unconditionally
/// (unlike `MACAddressPolicy=persistent`, which never fires on an adapter whose
/// `addr_assign_type` reads permanent while it actually randomizes).
pub fn render_link_file(match_block: &str, mac: &MacAddr) -> String {
    format!(
        "# Pin a stable MAC on an onboard adapter with no efuse MAC (it would\n\
# otherwise randomize each boot and churn the DHCP lease). Managed by the\n\
# ADOS agent; remove this file to revert. NamePolicy=kernel keeps the kernel\n\
# interface name so the wpa_supplicant binding survives.\n\
[Match]\n\
{}\n\
\n\
[Link]\n\
NamePolicy=kernel\n\
MACAddress={}\n",
        match_block.trim_end(),
        mac
    )
}

/// Extract the `[Match]` section body from a `.link` file (pure). Returns the
/// lines under `[Match]` up to the next `[Section]` or EOF, joined by newlines.
pub fn parse_match_block(link_body: &str) -> Option<String> {
    let mut in_match = false;
    let mut lines: Vec<&str> = Vec::new();
    for raw in link_body.lines() {
        let line = raw.trim();
        if line.starts_with('[') && line.ends_with(']') {
            if line.eq_ignore_ascii_case("[Match]") {
                in_match = true;
                continue;
            }
            if in_match {
                break; // next section ends the Match block
            }
            continue;
        }
        if in_match && !line.is_empty() && !line.starts_with('#') {
            lines.push(line);
        }
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

/// The pure per-adapter decision. `learner` is the adapter's record AFTER the
/// caller has folded in this boot's observation (so `mac_change_count` is
/// current). `salt` is empty for the only randomizer on the box, else the USB
/// path. Does no I/O.
#[allow(clippy::too_many_arguments)]
pub fn classify_adapter(
    usb_id: UsbId,
    iface: &str,
    vidpid: &str,
    machine_id: Option<&str>,
    salt: &str,
    config: &ReconcileConfig,
    networkd: bool,
    learner: Option<&LearnerRecord>,
    with_learner: bool,
) -> Decision {
    // 1. Operator override wins (by vidpid or by interface name).
    if let Some(raw) = config
        .overrides
        .get(vidpid)
        .or_else(|| config.overrides.get(iface))
    {
        return match (config.enabled, networkd, MacAddr::parse(raw)) {
            (false, _, _) => Decision::Disabled,
            (true, false, _) => Decision::Deferred("networkd not active".into()),
            (true, true, Some(mac)) => Decision::Pin {
                mac,
                source: AdapterSource::Override,
            },
            (true, true, None) => Decision::Deferred("override MAC is malformed".into()),
        };
    }

    // 2. Known-stable efuse radios are never touched.
    if is_known_stable_efuse(usb_id) {
        return Decision::Stable;
    }

    // 3. Known no-efuse randomizer (quirk table) -> auto-pin when armed.
    if is_quirk_randomizer(usb_id).is_some() {
        if !config.enabled {
            return Decision::Disabled;
        }
        if !networkd {
            return Decision::Deferred("networkd not active".into());
        }
        return match machine_id.and_then(|m| derive_pinned_mac(m, salt)) {
            Some(mac) => Decision::Pin {
                mac,
                source: AdapterSource::Quirk,
            },
            None => Decision::Deferred("no machine-id to derive a stable MAC".into()),
        };
    }

    // 4. Unknown adapter -> cross-boot learner (guided, never auto-pinned).
    if !with_learner {
        return Decision::Observe;
    }
    match learner {
        Some(rec) if rec.mac_change_count >= LEARN_THRESHOLD => {
            let proposed = machine_id.and_then(|m| derive_pinned_mac(m, salt));
            Decision::Candidate { proposed }
        }
        _ => Decision::Observe,
    }
}

// ── State file I/O (cross-platform; testable with tempdirs) ──────────────────

/// Read + parse the state file, returning the default (empty) state when it is
/// missing or malformed.
pub fn load_state_from(path: &Path) -> MacPinsState {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Atomically write the state file (tmp + rename).
pub fn save_state_to(path: &Path, state: &MacPinsState) -> std::io::Result<()> {
    let body = serde_json::to_vec_pretty(state).map_err(std::io::Error::other)?;
    atomic_write(path, &body, 0o644)
}

/// Convenience wrappers using [`STATE_PATH`].
pub fn load_state() -> MacPinsState {
    load_state_from(Path::new(STATE_PATH))
}
pub fn save_state(state: &MacPinsState) -> std::io::Result<()> {
    save_state_to(Path::new(STATE_PATH), state)
}

fn atomic_write(path: &Path, body: &[u8], mode: u32) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(body)?;
        f.flush()?;
        f.sync_all()?;
    }
    set_mode(&tmp, mode);
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}
#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) {}

/// Read the machine-id from the first available source.
pub fn read_machine_id() -> Option<String> {
    for p in MACHINE_ID_PATHS {
        if let Ok(s) = std::fs::read_to_string(p) {
            let t = s.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    None
}

// ── Linux device + networkd I/O ──────────────────────────────────────────────

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use crate::{AdapterState, AdapterVerdict};
    use std::path::PathBuf;
    use std::process::Command;

    fn now_unix() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// Enumerate USB-and-other network adapters under `/sys/class/net`. Skips
    /// loopback and interfaces with no backing device.
    pub fn enumerate_net_adapters() -> Vec<NetAdapter> {
        let mut out = Vec::new();
        let read = match std::fs::read_dir("/sys/class/net") {
            Ok(r) => r,
            Err(_) => return out,
        };
        for entry in read.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name == "lo" {
                continue;
            }
            let base = entry.path();
            let device = base.join("device");
            // No device link -> virtual interface; not a pin target.
            if !device.exists() {
                continue;
            }
            let (usb_id, usb_path) = resolve_usb_identity(&device);
            let mac = std::fs::read_to_string(base.join("address"))
                .ok()
                .and_then(|s| MacAddr::parse(s.trim()));
            out.push(NetAdapter {
                name,
                usb_id,
                usb_path,
                mac,
            });
        }
        out
    }

    /// Walk up from the interface's `device` link to the USB device node that
    /// carries `idVendor`/`idProduct`. Returns the id and the topology path
    /// (the device node's directory basename, e.g. `5-1.3`).
    fn resolve_usb_identity(device_link: &Path) -> (Option<UsbId>, String) {
        let mut cur = match std::fs::canonicalize(device_link) {
            Ok(p) => p,
            Err(_) => return (None, String::new()),
        };
        for _ in 0..6 {
            let vid = std::fs::read_to_string(cur.join("idVendor")).ok();
            let pid = std::fs::read_to_string(cur.join("idProduct")).ok();
            if let (Some(v), Some(p)) = (vid, pid) {
                let vid = u16::from_str_radix(v.trim(), 16).ok();
                let pid = u16::from_str_radix(p.trim(), 16).ok();
                let path = cur
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                if let (Some(vid), Some(pid)) = (vid, pid) {
                    return (Some(UsbId { vid, pid }), path);
                }
            }
            match cur.parent() {
                Some(p) => cur = p.to_path_buf(),
                None => break,
            }
        }
        (None, String::new())
    }

    /// True when `systemd-networkd` is managing the network (so a `.link` will
    /// be honored). `/run/systemd/netif` exists only while networkd runs.
    pub fn networkd_available() -> bool {
        Path::new("/run/systemd/netif").exists()
    }

    /// Ask `udevadm` which `.link` currently wins for an interface and return
    /// that file's `[Match]` block, so our drop-in claims the exact same adapter.
    /// `None` when udevadm is unavailable or the file cannot be read.
    pub fn winning_match_block(iface: &str) -> Option<String> {
        let target = format!("/sys/class/net/{iface}");
        let out = Command::new("udevadm")
            .args(["test-builtin", "net_setup_link", &target])
            .output()
            .ok()?;
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let path = text.lines().find_map(|l| {
            let l = l.trim();
            let i = l.find("Config file ")?;
            let rest = &l[i + "Config file ".len()..];
            let end = rest.find(" is applied")?;
            Some(rest[..end].trim().to_string())
        })?;
        let body = std::fs::read_to_string(&path).ok()?;
        parse_match_block(&body)
    }

    /// Resolve the `[Match]` block to use for `iface`: the winning stock file's
    /// match, or a fallback that matches this interface by its kernel name.
    pub fn resolve_match_block(iface: &str) -> String {
        winning_match_block(iface).unwrap_or_else(|| format!("OriginalName={iface}"))
    }

    /// Write the pin `.link` for `iface`. Idempotent: a no-op when the file
    /// already has identical content. Reloads udev so a later boot applies it;
    /// never touches the live interface. Returns the file path.
    pub fn write_pin_link(
        dir: &Path,
        iface: &str,
        match_block: &str,
        mac: &MacAddr,
    ) -> std::io::Result<PathBuf> {
        let path = dir.join(link_file_name(iface));
        let body = render_link_file(match_block, mac);
        let unchanged = std::fs::read_to_string(&path)
            .map(|cur| cur == body)
            .unwrap_or(false);
        if !unchanged {
            atomic_write(&path, body.as_bytes(), 0o644)?;
            reload_udev();
        }
        Ok(path)
    }

    /// Remove the pin `.link` for `iface`. Returns whether a file was removed.
    pub fn remove_pin_link(dir: &Path, iface: &str) -> std::io::Result<bool> {
        let path = dir.join(link_file_name(iface));
        if path.exists() {
            std::fs::remove_file(&path)?;
            reload_udev();
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Re-tag the LIVE interface now (drops any connection over it). Opt-in; the
    /// caller is responsible for the safety gate (never the management iface).
    pub fn apply_live(iface: &str, mac: &MacAddr) -> std::io::Result<()> {
        let mac = mac.to_string();
        run_ip(&["link", "set", "dev", iface, "down"])?;
        run_ip(&["link", "set", "dev", iface, "address", &mac])?;
        run_ip(&["link", "set", "dev", iface, "up"])?;
        Ok(())
    }

    fn run_ip(args: &[&str]) -> std::io::Result<()> {
        let status = Command::new("ip").args(args).status()?;
        if status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other(format!("ip {args:?} failed")))
        }
    }

    fn reload_udev() {
        let _ = Command::new("udevadm")
            .args(["control", "--reload"])
            .status();
        let _ = Command::new("udevadm")
            .args(["trigger", "--subsystem-match=net"])
            .status();
    }

    /// The full reconcile: enumerate -> fold the learner -> classify -> write
    /// `.link`s for armed quirk/override randomizers -> persist + return state.
    /// `with_learner` enables Layer-2 cross-boot detection (the supervisor runs
    /// it; the install step does not, to keep first-boot deterministic).
    pub fn reconcile(config: &ReconcileConfig, with_learner: bool) -> MacPinsState {
        let mut state = load_state();
        let adapters = enumerate_net_adapters();
        let machine_id = read_machine_id();
        let networkd = networkd_available();
        let now = now_unix();

        let quirk_count = adapters
            .iter()
            .filter(|a| {
                a.usb_id
                    .map(|id| is_quirk_randomizer(id).is_some())
                    .unwrap_or(false)
            })
            .count();

        let mut verdicts = Vec::new();
        for a in &adapters {
            let usb_id = match a.usb_id {
                Some(id) => id,
                None => continue, // skip non-USB
            };
            let vidpid = format!("{:04x}:{:04x}", usb_id.vid, usb_id.pid);
            let salt = if quirk_count > 1 {
                a.usb_path.as_str()
            } else {
                ""
            };

            // Fold this boot into the learner memory for unknown adapters (not
            // quirk, not known-efuse, not override). The record is keyed by the
            // stable identity so a churning MAC + name still resolves to it.
            let is_unknown = !config.overrides.contains_key(&vidpid)
                && !config.overrides.contains_key(&a.name)
                && !is_known_stable_efuse(usb_id)
                && is_quirk_randomizer(usb_id).is_none();
            if is_unknown {
                if let Some(mac) = a.mac {
                    fold_learner(&mut state.learner, &vidpid, &a.usb_path, mac, now);
                }
            }
            let learner_rec = state
                .learner
                .iter()
                .find(|r| r.vidpid == vidpid && r.usb_path == a.usb_path)
                .cloned();

            let decision = classify_adapter(
                usb_id,
                &a.name,
                &vidpid,
                machine_id.as_deref(),
                salt,
                config,
                networkd,
                learner_rec.as_ref(),
                with_learner,
            );
            verdicts.push(realize(decision, a, &vidpid));
        }
        state.adapters = verdicts;
        state.updated_at = now;
        let _ = save_state(&state);
        state
    }

    /// Turn a [`Decision`] into a verdict, writing the `.link` for a `Pin`.
    fn realize(decision: Decision, a: &NetAdapter, vidpid: &str) -> AdapterVerdict {
        let mut v = AdapterVerdict {
            name: a.name.clone(),
            vidpid: vidpid.to_string(),
            usb_path: a.usb_path.clone(),
            state: AdapterState::Stable,
            source: None,
            pinned_mac: None,
            last_seen_mac: a.mac,
            applied_live: false,
            link_file: None,
            deferred_reason: None,
        };
        match decision {
            Decision::Stable | Decision::Observe => {}
            Decision::Disabled => v.state = AdapterState::Disabled,
            Decision::Deferred(reason) => {
                v.state = AdapterState::Deferred;
                v.deferred_reason = Some(reason);
            }
            Decision::Candidate { proposed } => {
                v.state = AdapterState::Candidate;
                v.source = Some(AdapterSource::Learned);
                v.pinned_mac = proposed;
            }
            Decision::Pin { mac, source } => {
                let match_block = resolve_match_block(&a.name);
                match write_pin_link(Path::new(NETWORKD_DIR), &a.name, &match_block, &mac) {
                    Ok(path) => {
                        v.state = AdapterState::Pinned;
                        v.source = Some(source);
                        v.pinned_mac = Some(mac);
                        v.link_file = Some(path.to_string_lossy().to_string());
                        tracing::info!(
                            iface = %a.name, vidpid, mac = %mac,
                            "pinned a stable MAC on a no-efuse adapter (next boot)"
                        );
                    }
                    Err(e) => {
                        v.state = AdapterState::Deferred;
                        v.deferred_reason = Some(format!("link write failed: {e}"));
                        tracing::warn!(iface = %a.name, error = %e, "MAC pin link write failed");
                    }
                }
            }
        }
        v
    }
}

#[cfg(target_os = "linux")]
pub use linux::{
    apply_live, enumerate_net_adapters, networkd_available, reconcile, remove_pin_link,
    resolve_match_block, winning_match_block, write_pin_link,
};

// Non-Linux stubs so the crate builds + unit-tests on a dev host.
#[cfg(not(target_os = "linux"))]
pub fn enumerate_net_adapters() -> Vec<NetAdapter> {
    Vec::new()
}
#[cfg(not(target_os = "linux"))]
pub fn networkd_available() -> bool {
    false
}
#[cfg(not(target_os = "linux"))]
pub fn reconcile(_config: &ReconcileConfig, _with_learner: bool) -> MacPinsState {
    MacPinsState::default()
}

/// Fold one boot's observation into the learner memory (cross-platform so it is
/// unit-testable). Creates a record on first sight; on a later boot bumps
/// `boot_count` and, when the MAC changed, `mac_change_count`.
pub fn fold_learner(
    learner: &mut Vec<LearnerRecord>,
    vidpid: &str,
    usb_path: &str,
    current: MacAddr,
    now: u64,
) {
    if let Some(rec) = learner
        .iter_mut()
        .find(|r| r.vidpid == vidpid && r.usb_path == usb_path)
    {
        rec.boot_count = rec.boot_count.saturating_add(1);
        if rec.last_mac != current {
            rec.mac_change_count = rec.mac_change_count.saturating_add(1);
            rec.last_mac = current;
        }
    } else {
        learner.push(LearnerRecord {
            vidpid: vidpid.to_string(),
            usb_path: usb_path.to_string(),
            last_mac: current,
            first_seen: now,
            boot_count: 1,
            mac_change_count: 0,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(enabled: bool) -> ReconcileConfig {
        ReconcileConfig {
            enabled,
            apply_live_allowed: false,
            overrides: HashMap::new(),
        }
    }

    #[test]
    fn link_filename_sorts_before_stock() {
        assert_eq!(link_file_name("wlan0"), "10-ados-mac-wlan0.link");
        assert!("10-ados-mac-wlan0.link" < "50-radxa-aic8800.link");
    }

    #[test]
    fn render_carries_match_namepolicy_and_mac() {
        let mac = MacAddr::parse("02:c6:75:83:1a:3e").unwrap();
        let body = render_link_file("OriginalName=wlan*\nDriver=usb", &mac);
        assert!(body.contains("[Match]\nOriginalName=wlan*\nDriver=usb"));
        assert!(body.contains("NamePolicy=kernel"));
        assert!(body.contains("MACAddress=02:c6:75:83:1a:3e"));
    }

    #[test]
    fn parse_match_block_extracts_only_match_lines() {
        let body =
            "# comment\n[Match]\nOriginalName=wlan*\nDriver=usb\n\n[Link]\nNamePolicy=kernel\n";
        assert_eq!(
            parse_match_block(body).as_deref(),
            Some("OriginalName=wlan*\nDriver=usb")
        );
        // A round trip: render then re-parse yields the same match.
        let mac = MacAddr::parse("02:00:00:00:00:01").unwrap();
        let rendered = render_link_file("OriginalName=wlan0", &mac);
        assert_eq!(
            parse_match_block(&rendered).as_deref(),
            Some("OriginalName=wlan0")
        );
    }

    #[test]
    fn quirk_adapter_pins_when_armed() {
        let d = classify_adapter(
            UsbId {
                vid: 0xa69c,
                pid: 0x8d81,
            },
            "wlan0",
            "a69c:8d81",
            Some("03851cd61fc642d781d3f93a00e624cd"),
            "",
            &cfg(true),
            true,
            None,
            false,
        );
        assert_eq!(
            d,
            Decision::Pin {
                mac: MacAddr::parse("02:c6:75:83:1a:3e").unwrap(),
                source: AdapterSource::Quirk
            }
        );
    }

    #[test]
    fn quirk_adapter_deferred_or_disabled_when_blocked() {
        // disabled
        assert_eq!(
            classify_adapter(
                UsbId {
                    vid: 0xa69c,
                    pid: 1
                },
                "wlan0",
                "a69c:0001",
                Some("m"),
                "",
                &cfg(false),
                true,
                None,
                false
            ),
            Decision::Disabled
        );
        // no networkd
        assert!(matches!(
            classify_adapter(
                UsbId {
                    vid: 0xa69c,
                    pid: 1
                },
                "wlan0",
                "a69c:0001",
                Some("m"),
                "",
                &cfg(true),
                false,
                None,
                false
            ),
            Decision::Deferred(_)
        ));
        // no machine-id
        assert!(matches!(
            classify_adapter(
                UsbId {
                    vid: 0xa69c,
                    pid: 1
                },
                "wlan0",
                "a69c:0001",
                None,
                "",
                &cfg(true),
                true,
                None,
                false
            ),
            Decision::Deferred(_)
        ));
    }

    #[test]
    fn efuse_radio_is_left_stable() {
        let d = classify_adapter(
            UsbId {
                vid: 0x0bda,
                pid: 0xa81a,
            },
            "wlxabc",
            "0bda:a81a",
            Some("m"),
            "",
            &cfg(true),
            true,
            None,
            true,
        );
        assert_eq!(d, Decision::Stable);
    }

    #[test]
    fn override_pins_an_unknown_adapter() {
        let mut c = cfg(true);
        c.overrides
            .insert("1234:5678".into(), "02:11:22:33:44:55".into());
        let d = classify_adapter(
            UsbId {
                vid: 0x1234,
                pid: 0x5678,
            },
            "wlan1",
            "1234:5678",
            Some("m"),
            "",
            &c,
            true,
            None,
            true,
        );
        assert_eq!(
            d,
            Decision::Pin {
                mac: MacAddr::parse("02:11:22:33:44:55").unwrap(),
                source: AdapterSource::Override
            }
        );
    }

    #[test]
    fn unknown_adapter_only_candidate_after_threshold() {
        let id = UsbId {
            vid: 0x1234,
            pid: 0x5678,
        };
        // below threshold -> observe
        let rec = LearnerRecord {
            vidpid: "1234:5678".into(),
            usb_path: "1-1".into(),
            last_mac: MacAddr([0; 6]),
            first_seen: 0,
            boot_count: 3,
            mac_change_count: 1,
        };
        assert_eq!(
            classify_adapter(
                id,
                "wlan1",
                "1234:5678",
                Some("m"),
                "",
                &cfg(true),
                true,
                Some(&rec),
                true
            ),
            Decision::Observe
        );
        // at threshold -> candidate with a proposed MAC
        let rec2 = LearnerRecord {
            mac_change_count: LEARN_THRESHOLD,
            ..rec.clone()
        };
        assert!(matches!(
            classify_adapter(
                id,
                "wlan1",
                "1234:5678",
                Some("m"),
                "",
                &cfg(true),
                true,
                Some(&rec2),
                true
            ),
            Decision::Candidate { proposed: Some(_) }
        ));
        // learner disabled -> observe regardless
        assert_eq!(
            classify_adapter(
                id,
                "wlan1",
                "1234:5678",
                Some("m"),
                "",
                &cfg(true),
                true,
                Some(&rec2),
                false
            ),
            Decision::Observe
        );
    }

    #[test]
    fn fold_learner_tracks_changes() {
        let mut l = Vec::new();
        let a = MacAddr([1, 2, 3, 4, 5, 6]);
        let b = MacAddr([9, 8, 7, 6, 5, 4]);
        fold_learner(&mut l, "v:p", "1-1", a, 100); // first sight
        assert_eq!(l[0].boot_count, 1);
        assert_eq!(l[0].mac_change_count, 0);
        fold_learner(&mut l, "v:p", "1-1", b, 200); // changed
        assert_eq!(l[0].boot_count, 2);
        assert_eq!(l[0].mac_change_count, 1);
        fold_learner(&mut l, "v:p", "1-1", b, 300); // same
        assert_eq!(l[0].boot_count, 3);
        assert_eq!(l[0].mac_change_count, 1);
    }

    #[test]
    fn state_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mac-pins.state");
        let mut st = MacPinsState::default();
        st.learner.push(LearnerRecord {
            vidpid: "a:b".into(),
            usb_path: "1-1".into(),
            last_mac: MacAddr([0; 6]),
            first_seen: 1,
            boot_count: 1,
            mac_change_count: 0,
        });
        save_state_to(&path, &st).unwrap();
        assert_eq!(load_state_from(&path), st);
        // missing file -> default
        assert_eq!(
            load_state_from(&dir.path().join("nope")),
            MacPinsState::default()
        );
    }
}
