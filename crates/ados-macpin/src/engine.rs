//! The pinning engine: sysfs enumeration, the per-adapter decision, and the
//! `systemd-networkd` `.link` provisioning. The pure parts (`render_link_file`,
//! `parse_match_block`, `match_block_is_specific`, `parse_udev_id_path`,
//! `classify_adapter`) are unit-tested without I/O; the sysfs / `udevadm` /
//! subprocess parts are Linux-gated.
//!
//! The provisioning path only ever writes a `.link` file (effective on the next
//! boot). It never changes a live interface's address, so it cannot drop a
//! management link. Re-tagging the live interface is a separate, caller-gated
//! action ([`apply_live`]).
//!
//! Two entry points are meant for async request handlers and are therefore
//! `async` with bounded, `kill_on_drop` subprocesses ([`apply_live`],
//! [`remove_pin_link`]); the synchronous [`reconcile`] path is meant for a
//! provisioning step or a background service running off the async reactor.
//!
//! Every on-disk location (state file, drop-in directory, file-name prefix and
//! the owner named in the generated file header) comes from a [`PinLayout`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::atomic::write_atomic;
use crate::{
    derive_pinned_mac, is_known_stable_efuse, is_quirk_randomizer, AdapterSource, LearnerRecord,
    MacAddr, MacPinsState, UsbId,
};

/// The directory `systemd-networkd` reads `.link` drop-ins from on a stock
/// systemd host; a sensible [`PinLayout::networkd_dir`].
pub const NETWORKD_DIR: &str = "/etc/systemd/network";
/// Machine-id sources, in preference order.
pub const MACHINE_ID_PATHS: [&str; 2] = ["/etc/machine-id", "/var/lib/dbus/machine-id"];
/// The learner flags a randomizer after the MAC has changed on this many boots.
pub const LEARN_THRESHOLD: u32 = 2;

/// Where the engine keeps its files and how it names them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinLayout {
    /// The JSON state file holding the per-adapter verdicts and learner memory.
    pub state_path: PathBuf,
    /// The `systemd-networkd` drop-in directory the `.link` files go in
    /// (usually [`NETWORKD_DIR`]).
    pub networkd_dir: PathBuf,
    /// The file-name prefix every pin `.link` carries, e.g. `10-myapp-mac-`.
    /// A low number sorts before stock `.link` files so the pin wins; the
    /// prefix also tells the engine's own files apart from everything else in
    /// the directory.
    pub link_prefix: String,
    /// Who manages the files, named in the generated header
    /// (`# Managed by the {owner_label}`).
    pub owner_label: String,
}

/// A network adapter discovered under `/sys/class/net`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetAdapter {
    /// Kernel interface name (e.g. `wlan0`).
    pub name: String,
    /// `None` for non-USB adapters (platform NICs, virtual interfaces).
    pub usb_id: Option<UsbId>,
    /// USB topology path (e.g. `5-1.3`), port-stable across reboots. Empty when
    /// `usb_id` is `None`.
    pub usb_path: String,
    /// Current hardware address, if readable.
    pub mac: Option<MacAddr>,
}

/// Runtime knobs for one reconcile pass.
#[derive(Debug, Clone)]
pub struct ReconcileConfig {
    /// Master switch: when false, randomizers are reported but never pinned.
    pub enabled: bool,
    /// Whether the caller allows re-tagging a live interface.
    pub apply_live_allowed: bool,
    /// Operator overrides -> explicit MAC, keyed by a stable identity: the
    /// adapter key `vvvv:pppp@<usb_path>` (one adapter in one port), or a bare
    /// `vvvv:pppp` (every adapter of that model). Interface names are not
    /// identities and are not honoured.
    pub overrides: HashMap<String, String>,
    /// File locations and naming.
    pub layout: PinLayout,
}

/// The decision for one adapter (pure, no I/O).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Stable efuse MAC: leave it alone.
    Stable,
    /// Pinning disabled by config.
    Disabled,
    /// A randomizer we cannot pin right now.
    Deferred(String),
    /// Write a `.link` pinning `mac`.
    Pin {
        /// The address to pin.
        mac: MacAddr,
        /// How the verdict was reached.
        source: AdapterSource,
    },
    /// The learner suspects randomization; surface for the operator to confirm.
    Candidate {
        /// The MAC that would be pinned, when a machine-id is available.
        proposed: Option<MacAddr>,
    },
    /// Still gathering cross-boot evidence; no verdict yet.
    Observe,
}

// ── Pure helpers ────────────────────────────────────────────────────────────

/// The stable identity of a USB adapter: its model plus the port it sits in.
/// Survives a kernel rename and enumeration-order changes; two adapters of
/// the same model in different ports get different keys.
pub fn adapter_key(vidpid: &str, usb_path: &str) -> String {
    format!("{vidpid}@{usb_path}")
}

/// The `.link` filename for an adapter key (see [`adapter_key`]): the
/// layout's prefix plus the key. Characters outside `[A-Za-z0-9.@-]` become
/// `-`, so the name is always a plain file.
pub fn link_file_name(layout: &PinLayout, key: &str) -> String {
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
    format!("{}{safe}.link", layout.link_prefix)
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

/// The pin `.link` files to remove (pure). `existing` are the file names in
/// the drop-in directory, `present` the file names for every adapter on the
/// box now, `pinned` those this pass pinned. A file for a present adapter that
/// is no longer pinned is stale, and so is any pin file not named by an
/// adapter key (a legacy interface-name file pins whichever adapter holds that
/// name). A keyed file for an adapter that is merely unplugged is kept, so it
/// still applies when the adapter comes back.
pub fn stale_pin_links(
    layout: &PinLayout,
    existing: &[String],
    present: &std::collections::HashSet<String>,
    pinned: &std::collections::HashSet<String>,
) -> Vec<String> {
    existing
        .iter()
        .filter(|name| is_pin_link_file(layout, name) && !pinned.contains(*name))
        .filter(|name| present.contains(*name) || !name.contains('@'))
        .cloned()
        .collect()
}

/// Whether `name` is a pin `.link` file this engine writes under `layout`
/// (so an uninstall can sweep them without touching any other `.link` file).
pub fn is_pin_link_file(layout: &PinLayout, name: &str) -> bool {
    name.starts_with(&layout.link_prefix) && name.ends_with(".link")
}

mod classify;
mod link_file;

pub use classify::classify_adapter;
pub use link_file::{
    match_block_is_specific, parse_match_block, parse_udev_id_path, render_link_file,
};

// ── State file I/O (cross-platform; testable with tempdirs) ──────────────────

/// Read + parse the state file, returning the default (empty) state when it is
/// missing or malformed.
pub fn load_state_from(path: &Path) -> MacPinsState {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Atomically write the state file (temp file plus rename).
pub fn save_state_to(path: &Path, state: &MacPinsState) -> std::io::Result<()> {
    let body = serde_json::to_vec_pretty(state).map_err(std::io::Error::other)?;
    write_atomic(path, &body, 0o644)
}

mod projection;
pub use projection::{adapter_to_camel, adapters_camel_from, read_state, state_json_to_camel};

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
/// DHCP lease, churning the operator's management link mid-run. The pin is
/// meant to take effect on the next boot (per the module contract above), so no
/// live trigger is ever needed here. Kept at module scope (not inside the
/// Linux-gated `mod linux`) so the invariant is unit-testable on every
/// platform.
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
mod linux;

#[cfg(target_os = "linux")]
pub use linux::{
    apply_live, enumerate_net_adapters, link_mechanism_available, reconcile, remove_pin_link,
    resolve_match_block, write_pin_link,
};

// Non-Linux stubs so the crate builds + unit-tests on a dev host.
/// Non-Linux stub: no sysfs, so no adapters.
#[cfg(not(target_os = "linux"))]
pub fn enumerate_net_adapters() -> Vec<NetAdapter> {
    Vec::new()
}
/// Non-Linux stub: always `false`.
#[cfg(not(target_os = "linux"))]
pub fn link_mechanism_available() -> bool {
    false
}
/// Non-Linux stub: does nothing and returns an empty state.
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
mod tests;
