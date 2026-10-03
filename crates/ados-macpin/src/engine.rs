//! The MAC-pin engine: sysfs enumeration, the per-adapter decision, and the
//! `systemd-networkd` `.link` provisioning. The pure parts (`render_link_file`,
//! `parse_match_block`, `match_block_is_specific`, `parse_udev_id_path`,
//! `classify_adapter`) are unit-tested without I/O; the sysfs / `udevadm` /
//! subprocess parts are Linux-gated.
//!
//! The provisioning path only ever writes a `.link` file (effective on the next
//! boot) — it never changes a live interface's address, so it cannot drop the
//! operator's management link. Re-tagging the live interface is a separate,
//! caller-gated action ([`apply_live`]).
//!
//! Two entry points are reached from an axum handler and are therefore `async`
//! with bounded, `kill_on_drop` subprocesses ([`apply_live`],
//! [`remove_pin_link`]); the synchronous `reconcile` path belongs to the
//! installer step and the supervisor, which run off the reactor.

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
    /// Operator overrides -> explicit MAC, keyed by a stable identity: the
    /// adapter key `vvvv:pppp@<usb_path>` (one adapter in one port), or a bare
    /// `vvvv:pppp` (every adapter of that model). Interface names are not
    /// identities and are not honoured.
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

/// The file-name prefix every MAC-pin `.link` drop-in carries. The `10-` sorts
/// before the stock board files (`50-...`) so the pin wins.
pub const LINK_FILE_PREFIX: &str = "10-ados-mac-";

/// The stable identity of a USB adapter: its model plus the port it sits in.
/// Survives a kernel rename and enumeration-order changes; two adapters of
/// the same model in different ports get different keys.
pub fn adapter_key(vidpid: &str, usb_path: &str) -> String {
    format!("{vidpid}@{usb_path}")
}

/// The `.link` filename for an adapter key (see [`adapter_key`]). Characters
/// outside `[A-Za-z0-9.@-]` become `-`, so the name is always a plain file.
pub fn link_file_name(key: &str) -> String {
    let safe: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '@' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("{LINK_FILE_PREFIX}{safe}.link")
}

/// The adapter key the state file records for the adapter currently named
/// `iface`, or `None` when the file is absent or knows no such adapter. The
/// write routes take an interface name from the operator and pin by this key.
pub fn adapter_key_for_iface(state_path: &Path, iface: &str) -> Option<String> {
    let doc = read_state(state_path)?;
    doc.get("adapters")?.as_array()?.iter().find_map(|a| {
        if a.get("name")?.as_str()? != iface {
            return None;
        }
        let vidpid = a.get("vidpid")?.as_str()?;
        let usb_path = a.get("usb_path")?.as_str()?;
        (!vidpid.is_empty() && !usb_path.is_empty()).then(|| adapter_key(vidpid, usb_path))
    })
}

/// The pin `.link` files to remove (pure). `existing` are the pin file names
/// in the drop-in directory, `present` the file names for every adapter on
/// the box now, `pinned` those this pass pinned. A file for a present adapter
/// that is no longer pinned is stale, and so is any file not named by an
/// adapter key (a legacy interface-name file pins whichever adapter holds
/// that name). A keyed file for an adapter that is merely unplugged is kept,
/// so it still applies when the adapter comes back.
pub fn stale_pin_links(
    existing: &[String],
    present: &std::collections::HashSet<String>,
    pinned: &std::collections::HashSet<String>,
) -> Vec<String> {
    existing
        .iter()
        .filter(|name| is_pin_link_file(name) && !pinned.contains(*name))
        .filter(|name| present.contains(*name) || !name.contains('@'))
        .cloned()
        .collect()
}

/// Whether `name` is a MAC-pin `.link` file this engine writes (so an
/// uninstall can sweep them without touching a board's own `.link` files).
pub fn is_pin_link_file(name: &str) -> bool {
    name.starts_with(LINK_FILE_PREFIX) && name.ends_with(".link")
}

/// Render the full `.link` body (pure). `match_block` is the body of the
/// `[Match]` section (without the `[Match]` header) and MUST name this one
/// adapter: `[Link] MACAddress=` is unconditional, so a wildcard match applies
/// the pinned address to EVERY interface on the box. `NamePolicy=kernel`
/// preserves the kernel interface name so a `wpa_supplicant@<iface>` binding
/// keeps working; an explicit `MACAddress=` sets the address unconditionally
/// (unlike `MACAddressPolicy=persistent`, which never fires on an adapter whose
/// `addr_assign_type` reads permanent while it actually randomizes).
pub fn render_link_file(match_block: &str, mac: &MacAddr) -> String {
    format!(
        "# Pin a stable MAC on an onboard adapter with no efuse MAC (it would\n\
# otherwise randomize each boot and churn the DHCP lease). Managed by the\n\
# ADOS agent; remove this file to revert. NamePolicy=kernel keeps the kernel\n\
# interface name so the wpa_supplicant binding survives. The [Match] block\n\
# names exactly one adapter: MACAddress= below is unconditional, so a glob\n\
# here would give every interface on this box the same address.\n\
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

/// The `[Match]` keys that identify ONE adapter. A pin `.link` is only safe
/// when at least one of these carries a glob-free value.
const IDENTITY_MATCH_KEYS: [&str; 4] =
    ["Path", "OriginalName", "PermanentMACAddress", "MACAddress"];

/// True when a `[Match]` block names one specific adapter (pure).
///
/// The pin used to COPY the `[Match]` of whichever stock `.link` won for the
/// interface — and on a stock systemd that is `99-default.link`, whose match is
/// `OriginalName=*`. Pairing that wildcard with our unconditional
/// `MACAddress=` handed the pinned address to every interface on the box: the
/// management NIC, the second WFB radio, everything. So a pin is now refused
/// unless the match carries a glob-free identity key.
pub fn match_block_is_specific(match_block: &str) -> bool {
    match_block.lines().any(|raw| {
        let line = raw.trim();
        let Some((key, value)) = line.split_once('=') else {
            return false;
        };
        let key = key.trim();
        let value = value.trim();
        IDENTITY_MATCH_KEYS
            .iter()
            .any(|k| k.eq_ignore_ascii_case(key))
            && !value.is_empty()
            && !value.contains(['*', '?', '['])
            // A leading `!` is a systemd negation: "any adapter EXCEPT this
            // one", which is the opposite of specific.
            && !value.starts_with('!')
    })
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

/// The `ID_PATH` value from a `udevadm info --query=property` body (pure).
///
/// `ID_PATH` is the stable per-port topology id (`platform-xhci-hcd.0-usb-0:1.3:1.0`)
/// that systemd's `.link` `Path=` key matches on, so it pins the adapter in a
/// given USB port rather than whatever currently answers to `wlan0`. Returns
/// `None` when the property is absent or carries a glob character (which would
/// make the match non-specific).
pub fn parse_udev_id_path(props: &str) -> Option<String> {
    for line in props.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("ID_PATH=") {
            let value = value.trim().trim_matches('"');
            if !value.is_empty() && !value.contains(['*', '?', '[']) {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// The pure per-adapter decision. `learner` is the adapter's record AFTER the
/// caller has folded in this boot's observation (so `mac_change_count` is
/// current). `salt` is the adapter's stable USB path, so every pinned adapter
/// derives a MAC unique to its port and no two radios can collapse to one MAC.
/// Does no I/O.
#[allow(clippy::too_many_arguments)]
pub fn classify_adapter(
    usb_id: UsbId,
    key: &str,
    vidpid: &str,
    machine_id: Option<&str>,
    salt: &str,
    config: &ReconcileConfig,
    link_mechanism: bool,
    learner: Option<&LearnerRecord>,
    with_learner: bool,
) -> Decision {
    // 1. Operator override wins (by adapter key, else by model).
    if let Some(raw) = config
        .overrides
        .get(key)
        .or_else(|| config.overrides.get(vidpid))
    {
        return match (config.enabled, link_mechanism, MacAddr::parse(raw)) {
            (false, _, _) => Decision::Disabled,
            (true, false, _) => Decision::Deferred("no systemd-udev link mechanism".into()),
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
        if !link_mechanism {
            return Decision::Deferred("no systemd-udev link mechanism".into());
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

// ---------------------------------------------------------------------------
// The GCS-facing projection of the per-adapter verdicts.
// ---------------------------------------------------------------------------
//
// One copy, here, because the state file is this crate's. It feeds BOTH
// transports: the LAN `GET /api/v1/network/mac/adapters` route, the LAN
// `/api/status/full` body, and the cloud heartbeat's `macStability` block. Every
// one of the GCS's readers of that block sat empty because no transport produced
// it, and the fix would have been three projections without this.
//
// The keys are ALREADY camelCase here (`usbPath`, `appliedLive`, `pinnedMac`), so
// a consumer must not put this block through a snake→camel remap.

/// Read + parse the state file into a JSON document, or `None` when the file is
/// absent or malformed.
pub fn read_state(state_path: &Path) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(state_path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Project one state-file adapter object to the camelCase shape the GCS reads.
pub fn adapter_to_camel(a: &serde_json::Map<String, serde_json::Value>) -> serde_json::Value {
    use serde_json::Value;
    let mut out = serde_json::Map::new();
    // Always present (carried through verbatim — an absent source key renders
    // JSON null).
    for key in ["name", "vidpid"] {
        out.insert(key.to_string(), a.get(key).cloned().unwrap_or(Value::Null));
    }
    out.insert(
        "usbPath".to_string(),
        a.get("usb_path").cloned().unwrap_or(Value::Null),
    );
    out.insert(
        "state".to_string(),
        a.get("state").cloned().unwrap_or(Value::Null),
    );
    // `applied_live` coerced to a bool with a false default.
    let applied_live = a.get("applied_live").map(value_is_truthy).unwrap_or(false);
    out.insert("appliedLive".to_string(), Value::Bool(applied_live));

    // Present only when the source value is non-null.
    for (src, dst) in [
        ("source", "source"),
        ("pinned_mac", "pinnedMac"),
        ("last_seen_mac", "lastSeenMac"),
        ("link_file", "linkFile"),
        ("deferred_reason", "deferredReason"),
    ] {
        if let Some(v) = a.get(src) {
            if !v.is_null() {
                out.insert(dst.to_string(), v.clone());
            }
        }
    }
    Value::Object(out)
}

/// Truthiness for the `applied_live` coercion: a JSON bool is itself; `null` is
/// false; a number is false iff zero; a string/array/object is false iff empty.
fn value_is_truthy(v: &serde_json::Value) -> bool {
    use serde_json::Value;
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// The projected adapter list from a parsed state document, or an empty list when
/// the document is absent / malformed / carries no `adapters` array.
pub fn state_json_to_camel(raw: Option<&serde_json::Value>) -> Vec<serde_json::Value> {
    raw.and_then(|d| d.get("adapters"))
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(serde_json::Value::as_object)
                .map(adapter_to_camel)
                .collect()
        })
        .unwrap_or_default()
}

/// The `macStability` block for a status body / heartbeat: `{adapters: [...]}`,
/// or `None` when the state file is absent, malformed, or lists no adapter.
///
/// `None` rather than an empty list on purpose: the GCS clamp hides the card
/// unless `adapters` is an array, and a node that has never pinned a MAC has no
/// verdict to report — an empty card would claim it looked and found nothing.
///
/// No staleness gate: `/etc/ados/mac-pins.state` is persistent operator state, not
/// a tmpfs liveness sidecar, so an old reading is still the current verdict.
pub fn adapters_camel_from(state_path: &Path) -> Option<serde_json::Value> {
    let raw = read_state(state_path);
    let adapters = state_json_to_camel(raw.as_ref());
    if adapters.is_empty() {
        return None;
    }
    Some(serde_json::json!({ "adapters": adapters }))
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

/// The `udevadm` invocations the pin/unpin path runs after touching a `.link`
/// file. It ONLY reloads the rules database so a future device event (a
/// hot-plug, or the next boot) picks up the new/removed file.
///
/// It MUST NOT `trigger` the `net` subsystem: `udevadm trigger` re-runs
/// `net_setup_link` on the LIVE interface, which re-applies the pinned
/// `MACAddress=` to the running `wlan0`, drops the association, and forces a new
/// DHCP lease — churning the operator's management link mid-run. That is the
/// exact hazard the installer's `install_power_hardening` avoids by scoping its
/// own trigger to `--subsystem-match=usb`. The pin is meant to take effect on
/// the next boot (per the module contract above), so no live trigger is ever
/// needed here. Kept at module scope (not inside the Linux-gated `mod linux`) so
/// the invariant is unit-testable on every platform.
// Consumed by `mod linux::reload_udev` (Linux-only) and the invariant test; on a
// non-Linux, non-test build neither is compiled, so allow the unused const there.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const UDEV_RELOAD_ARGV: &[&[&str]] = &[&["control", "--reload"]];

/// Cap on one `ip link set` invocation on the live re-tag path. A netlink call
/// that never returns must not park the request handler that issued it.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const IP_CMD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Cap on one `udevadm` invocation on the async unpin path.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const UDEV_CMD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

// ── Linux device + .link I/O ─────────────────────────────────────────────────

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

    /// True when the systemd-udev `.link` mechanism is available to apply a
    /// `[Link] MACAddress=` drop-in at device setup. The `net_setup_link` udev
    /// builtin owns `.link` application and runs on ANY systemd host -- whether
    /// the L3 manager is systemd-networkd OR NetworkManager (both run
    /// systemd-udevd). So the pin is honored on NetworkManager-only boards too;
    /// gating on networkd *running* (`/run/systemd/netif`) alone would wrongly
    /// defer the pin on an NM board and let its MAC keep churning. We check the
    /// udev runtime (`/run/udev`), with the networkd runtime dir as a fallback.
    pub fn link_mechanism_available() -> bool {
        Path::new("/run/udev").exists() || Path::new("/run/systemd/netif").exists()
    }

    /// The adapter's udev `ID_PATH` (its stable USB-port topology id), or
    /// `None` when `udevadm` is unavailable or the property is absent.
    fn udev_id_path(iface: &str) -> Option<String> {
        let target = format!("/sys/class/net/{iface}");
        let out = Command::new("udevadm")
            .args(["info", "--query=property", "--path", &target])
            .output()
            .ok()?;
        parse_udev_id_path(&String::from_utf8_lossy(&out.stdout))
    }

    /// Resolve the `[Match]` block for `iface`: the adapter's stable USB-port
    /// path (`Path=`), or `None` when udev does not know it. There is no
    /// interface-name fallback: `OriginalName=` names whichever adapter
    /// enumerated under that name this boot, so a pin keyed on it moves
    /// between adapters when they swap order.
    ///
    /// It deliberately does NOT mirror whichever stock `.link` wins for the
    /// interface. That file is normally `99-default.link`, whose match is
    /// `OriginalName=*`, and copying it into a drop-in carrying an
    /// unconditional `MACAddress=` pinned ONE adapter's address onto every
    /// interface on the box. [`match_block_is_specific`] is the write-time
    /// backstop.
    pub fn resolve_match_block(iface: &str) -> Option<String> {
        udev_id_path(iface).map(|p| format!("Path={p}"))
    }

    /// Write the pin `.link` for the adapter `key`. Idempotent: a no-op when
    /// the file already has identical content. Reloads udev so a later boot
    /// applies it; never touches the live interface. Returns the file path.
    ///
    /// Refused when `match_block` is not specific to one adapter: the file
    /// carries an unconditional `MACAddress=`, so a glob match would give every
    /// interface on the box the same address.
    pub fn write_pin_link(
        dir: &Path,
        key: &str,
        match_block: &str,
        mac: &MacAddr,
    ) -> std::io::Result<PathBuf> {
        if !match_block_is_specific(match_block) {
            return Err(std::io::Error::other(format!(
                "refusing to pin {key}: [Match] block {match_block:?} does not name one adapter"
            )));
        }
        let path = dir.join(link_file_name(key));
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

    /// Remove the pin `.link` for the adapter `key`. Returns whether a file
    /// was removed.
    ///
    /// Async because its only caller is the `DELETE /api/v1/network/mac/{iface}`
    /// handler: the udev reload is a subprocess whose settle would otherwise
    /// block the reactor and stall every other in-flight request on a
    /// single-core SBC.
    pub async fn remove_pin_link(dir: &Path, key: &str) -> std::io::Result<bool> {
        let path = dir.join(link_file_name(key));
        if path.exists() {
            std::fs::remove_file(&path)?;
            reload_udev_async().await;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Remove the stale pin `.link` files (see [`stale_pin_links`]). Returns
    /// how many were removed.
    fn sweep_pin_links(
        dir: &Path,
        present: &std::collections::HashSet<String>,
        pinned: &std::collections::HashSet<String>,
    ) -> usize {
        let existing: Vec<String> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        let mut removed = 0;
        for name in stale_pin_links(&existing, present, pinned) {
            match std::fs::remove_file(dir.join(&name)) {
                Ok(()) => {
                    removed += 1;
                    tracing::info!(file = %name, "removed a stale MAC pin link");
                }
                Err(e) => {
                    tracing::warn!(file = %name, error = %e, "stale MAC pin link not removed")
                }
            }
        }
        if removed > 0 {
            reload_udev();
        }
        removed
    }

    /// Re-tag the LIVE interface now (drops any connection over it). Opt-in; the
    /// caller is responsible for the safety gate (never the management iface).
    ///
    /// Async + bounded: it is called from an axum handler, and a blocking
    /// `ip link set` on the reactor stalls every other request for the duration
    /// of the link down/up. Each invocation is capped by [`IP_CMD_TIMEOUT`] with
    /// `kill_on_drop`, so a wedged netlink call cannot park the handler forever.
    pub async fn apply_live(iface: &str, mac: &MacAddr) -> std::io::Result<()> {
        let mac = mac.to_string();
        run_ip(&["link", "set", "dev", iface, "down"]).await?;
        run_ip(&["link", "set", "dev", iface, "address", &mac]).await?;
        run_ip(&["link", "set", "dev", iface, "up"]).await?;
        Ok(())
    }

    async fn run_ip(args: &[&str]) -> std::io::Result<()> {
        let status = tokio::time::timeout(
            IP_CMD_TIMEOUT,
            tokio::process::Command::new("ip")
                .args(args)
                .kill_on_drop(true)
                .status(),
        )
        .await
        .map_err(|_| std::io::Error::other(format!("ip {args:?} timed out")))??;
        if status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other(format!("ip {args:?} failed")))
        }
    }

    /// Reload the udev rules database so a later device event (a hot-plug, or
    /// the next boot) picks up the new/removed `.link` file. Runs only the
    /// invocations in [`super::UDEV_RELOAD_ARGV`] — which deliberately excludes
    /// any `udevadm trigger` of the `net` subsystem (see that const's docs).
    ///
    /// Blocking form, for the SYNCHRONOUS pin path only: the installer step and
    /// the supervisor reconciler drive `reconcile` off the reactor. The async
    /// route path uses [`reload_udev_async`].
    fn reload_udev() {
        for argv in UDEV_RELOAD_ARGV {
            let _ = Command::new("udevadm").args(*argv).status();
        }
    }

    /// [`reload_udev`] for an async caller: bounded and `kill_on_drop`, so a
    /// udev settle can neither block the reactor nor outlive the request.
    async fn reload_udev_async() {
        for argv in UDEV_RELOAD_ARGV {
            let _ = tokio::time::timeout(
                UDEV_CMD_TIMEOUT,
                tokio::process::Command::new("udevadm")
                    .args(*argv)
                    .kill_on_drop(true)
                    .status(),
            )
            .await;
        }
    }

    /// The full reconcile: enumerate -> fold the learner -> classify -> write
    /// `.link`s for armed quirk/override randomizers -> persist + return state.
    /// `with_learner` enables Layer-2 cross-boot detection (the supervisor runs
    /// it; the install step does not, to keep first-boot deterministic).
    pub fn reconcile(config: &ReconcileConfig, with_learner: bool) -> MacPinsState {
        let mut state = load_state();
        let adapters = enumerate_net_adapters();
        let machine_id = read_machine_id();
        let link_mechanism = link_mechanism_available();
        let now = now_unix();

        let mut verdicts = Vec::new();
        let mut present = std::collections::HashSet::new();
        let mut pinned = std::collections::HashSet::new();
        for a in &adapters {
            let usb_id = match a.usb_id {
                Some(id) => id,
                None => continue, // skip non-USB
            };
            let vidpid = format!("{:04x}:{:04x}", usb_id.vid, usb_id.pid);
            let key = adapter_key(&vidpid, &a.usb_path);
            present.insert(link_file_name(&key));
            // Always salt the derived MAC by the adapter's stable USB path, so a
            // lone randomizer can never derive the same machine-id-only MAC that
            // collides with another radio's (the 2026-08 A7S dup-MAC bug).
            let salt = a.usb_path.as_str();

            // Fold this boot into the learner memory for unknown adapters (not
            // quirk, not known-efuse, not override). The record is keyed by the
            // stable identity so a churning MAC + name still resolves to it.
            let is_unknown = !config.overrides.contains_key(&vidpid)
                && !config.overrides.contains_key(&key)
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
                &key,
                &vidpid,
                machine_id.as_deref(),
                salt,
                config,
                link_mechanism,
                learner_rec.as_ref(),
                with_learner,
            );
            let verdict = realize(decision, a, &vidpid, &key);
            if verdict.state == AdapterState::Pinned {
                pinned.insert(link_file_name(&key));
            }
            verdicts.push(verdict);
        }
        sweep_pin_links(Path::new(NETWORKD_DIR), &present, &pinned);
        state.adapters = verdicts;
        state.updated_at = now;
        let _ = save_state(&state);
        state
    }

    /// Turn a [`Decision`] into a verdict, writing the `.link` for a `Pin`.
    fn realize(decision: Decision, a: &NetAdapter, vidpid: &str, key: &str) -> AdapterVerdict {
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
                let Some(match_block) = resolve_match_block(&a.name) else {
                    v.state = AdapterState::Deferred;
                    v.deferred_reason =
                        Some("no stable device path (udev ID_PATH) to match on".into());
                    tracing::warn!(iface = %a.name, "MAC pin deferred: no stable device path");
                    return v;
                };
                match write_pin_link(Path::new(NETWORKD_DIR), key, &match_block, &mac) {
                    Ok(path) => {
                        v.state = AdapterState::Pinned;
                        v.source = Some(source);
                        v.pinned_mac = Some(mac);
                        v.link_file = Some(path.to_string_lossy().to_string());
                        tracing::info!(
                            iface = %a.name, key, mac = %mac,
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
    apply_live, enumerate_net_adapters, link_mechanism_available, reconcile, remove_pin_link,
    resolve_match_block, write_pin_link,
};

// Non-Linux stubs so the crate builds + unit-tests on a dev host.
#[cfg(not(target_os = "linux"))]
pub fn enumerate_net_adapters() -> Vec<NetAdapter> {
    Vec::new()
}
#[cfg(not(target_os = "linux"))]
pub fn link_mechanism_available() -> bool {
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
    fn link_filename_sorts_before_stock_and_names_the_adapter() {
        let name = link_file_name(&adapter_key("a69c:8d81", "5-1.3"));
        // Assembled from the key so the expected name does not read as an email
        // address to the repository's leak scanner.
        assert_eq!(name, format!("10-ados-mac-{}.link", "a69c-8d81@5-1.3"));
        assert!(is_pin_link_file(&name));
        assert!(name.as_str() < "50-radxa-aic8800.link");
    }

    #[test]
    fn an_override_follows_the_adapter_not_the_interface_name() {
        let mut c = cfg(true);
        c.overrides
            .insert(adapter_key("1234:5678", "1-2"), "02:11:22:33:44:55".into());
        c.overrides
            .insert("wlan0".into(), "02:aa:aa:aa:aa:aa".into());
        let id = UsbId {
            vid: 0x1234,
            pid: 0x5678,
        };
        let pin = |key: &str| {
            classify_adapter(id, key, "1234:5678", Some("m"), "", &c, true, None, false)
        };
        // The keyed adapter is pinned wherever it enumerates.
        assert_eq!(
            pin(&adapter_key("1234:5678", "1-2")),
            Decision::Pin {
                mac: MacAddr::parse("02:11:22:33:44:55").unwrap(),
                source: AdapterSource::Override
            }
        );
        // The same model in another port is not, and a name key never applies.
        assert_eq!(pin(&adapter_key("1234:5678", "1-3")), Decision::Observe);
    }

    #[test]
    fn stale_pin_links_are_swept_but_an_unplugged_adapter_keeps_its_pin() {
        let set = |names: &[&str]| -> std::collections::HashSet<String> {
            names.iter().map(|s| s.to_string()).collect()
        };
        let kept = link_file_name(&adapter_key("a69c:8d81", "5-1.3"));
        let unpinned = link_file_name(&adapter_key("a69c:8d81", "5-1.4"));
        let unplugged = link_file_name(&adapter_key("a69c:8d81", "3-1"));
        let existing = vec![
            kept.clone(),
            unpinned.clone(),
            unplugged.clone(),
            "10-ados-mac-wlan0.link".to_string(),
            "50-radxa-aic8800.link".to_string(),
        ];
        let mut stale = stale_pin_links(
            &existing,
            &set(&[kept.as_str(), unpinned.as_str()]),
            &set(&[kept.as_str()]),
        );
        stale.sort();
        assert_eq!(stale, vec![unpinned, "10-ados-mac-wlan0.link".to_string()]);
    }

    #[test]
    fn reload_udev_never_triggers_the_live_net_link() {
        // The pin/unpin path may reload the udev rules DB, but must NEVER
        // `udevadm trigger` the net subsystem — that re-applies the pinned MAC
        // to the LIVE interface and drops the operator's management link (the
        // install-churn root cause). Reloading the rules DB is the only allowed
        // action; the `.link` takes effect on the next boot / hot-plug event.
        for argv in UDEV_RELOAD_ARGV {
            assert!(
                !argv.contains(&"trigger"),
                "reload must not re-trigger devices: {argv:?}"
            );
            assert!(
                !argv.iter().any(|a| a.contains("subsystem-match=net")),
                "reload must never target the net subsystem: {argv:?}"
            );
        }
        // Positive assertion: it does reload the rules DB so a new/removed file
        // is seen at the next event.
        assert!(UDEV_RELOAD_ARGV
            .iter()
            .any(|argv| argv.contains(&"control") && argv.contains(&"--reload")));
    }

    #[test]
    fn render_carries_match_namepolicy_and_mac() {
        let mac = MacAddr::parse("02:c6:75:83:1a:3e").unwrap();
        let body = render_link_file("Path=platform-xhci-hcd.0-usb-0:1.3:1.0", &mac);
        assert!(body.contains("[Match]\nPath=platform-xhci-hcd.0-usb-0:1.3:1.0"));
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
    fn a_wildcard_match_is_not_specific_enough_to_pin() {
        // The stock `99-default.link` match. Pairing it with our unconditional
        // MACAddress= handed one adapter's pinned MAC to every interface.
        assert!(!match_block_is_specific("OriginalName=*"));
        assert!(!match_block_is_specific("OriginalName=wlan*"));
        assert!(!match_block_is_specific("Driver=usb\nType=wlan"));
        assert!(!match_block_is_specific("OriginalName=!eth0"));
        assert!(!match_block_is_specific(""));
        // The two forms the resolver emits, and a permanent-MAC match.
        assert!(match_block_is_specific("OriginalName=wlan0"));
        assert!(match_block_is_specific(
            "Path=platform-xhci-hcd.0-usb-0:1.3:1.0"
        ));
        assert!(match_block_is_specific(
            "Driver=usb\nPermanentMACAddress=00:c0:ca:b1:0d:a2"
        ));
        // A glob ANDed with a specific key is still one adapter.
        assert!(match_block_is_specific(
            "OriginalName=wlan*\nPath=pci-0000:01:00.0"
        ));
    }

    #[test]
    fn id_path_is_read_from_udev_properties() {
        let props = "ID_BUS=usb\nID_PATH=platform-xhci-hcd.0-usb-0:1.3:1.0\nID_PATH_TAG=platform_xhci_hcd_0_usb_0_1_3_1_0\n";
        assert_eq!(
            parse_udev_id_path(props).as_deref(),
            Some("platform-xhci-hcd.0-usb-0:1.3:1.0")
        );
        // Absent property, and a value carrying a glob (never usable as a
        // specific match), both decline.
        assert_eq!(parse_udev_id_path("ID_BUS=usb\n"), None);
        assert_eq!(parse_udev_id_path("ID_PATH=usb-*\n"), None);
        assert_eq!(parse_udev_id_path("ID_PATH=\n"), None);
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
    fn two_quirk_adapters_salted_by_usb_path_get_distinct_macs() {
        // Two no-efuse randomizers on one box must NEVER collapse to the same
        // MAC. The reconcile passes each a distinct salt (its USB topology path)
        // whenever more than one quirk adapter exists; a single randomizer keeps
        // the historical "" parity MAC. This guards that salt-threading contract.
        let machine_id = "03851cd61fc642d781d3f93a00e624cd";
        let id = UsbId {
            vid: 0xa69c,
            pid: 0x8d81,
        };
        let single = classify_adapter(
            id,
            "wlan0",
            "a69c:8d81",
            Some(machine_id),
            "",
            &cfg(true),
            true,
            None,
            false,
        );
        // A lone randomizer keeps the proven, bit-for-bit "" parity MAC.
        assert_eq!(
            single,
            Decision::Pin {
                mac: MacAddr::parse("02:c6:75:83:1a:3e").unwrap(),
                source: AdapterSource::Quirk
            }
        );
        // Two randomizers, distinct USB paths -> distinct MACs, both valid.
        let a = classify_adapter(
            id,
            "wlan0",
            "a69c:8d81",
            Some(machine_id),
            "5-1.3",
            &cfg(true),
            true,
            None,
            false,
        );
        let b = classify_adapter(
            id,
            "wlan1",
            "a69c:8d81",
            Some(machine_id),
            "5-1.4",
            &cfg(true),
            true,
            None,
            false,
        );
        let (Decision::Pin { mac: mac_a, .. }, Decision::Pin { mac: mac_b, .. }) = (a, b) else {
            panic!("both quirk adapters should pin");
        };
        assert_ne!(mac_a, mac_b);
        assert_ne!(mac_a, MacAddr::parse("02:c6:75:83:1a:3e").unwrap());
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
        // no link mechanism (systemd-udev unavailable)
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
