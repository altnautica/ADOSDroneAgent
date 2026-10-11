//! USB hot-plug detection by presence-transition polling.
//!
//! Polls a small set of device-class identity sets on an interval (1s on a
//! normal board, 10s on a low-RAM SBC, matching the Python monitor's swap
//! sensitivity tradeoff) and emits an event when any class's set changes. An
//! identity carries the USB device number, which the kernel reassigns on every
//! re-enumeration, so a swap, a partial removal, or an unplug and replug inside
//! one poll all read as a change. The first snapshot is the baseline, so
//! devices already present at boot do not fire.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::sync::mpsc::Sender;
use tokio::time::sleep;

use crate::hardware;

/// Device classes the supervisor restarts a service for on a hot-plug change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DevKind {
    Camera,
    Fc,
    Radio,
    /// An ExpressLRS / CRSF RC transmitter module's USB-serial bridge. Kept a
    /// separate class from [`DevKind::Fc`] with its own debounce entry, so
    /// plugging/unplugging the module never restarts the FC link service.
    Elrs,
}

/// Minimum spacing between two restarts for the same device class. A device
/// that re-enumerates (e.g. a flight controller dropping out of DFU into flight
/// firmware within ~1s) fires a remove + an add edge in quick succession;
/// without this window each edge would issue its own `systemctl restart` and
/// thrash the unit. Matches the per-device debounce the Python supervisor uses.
pub const HOTPLUG_DEBOUNCE: Duration = Duration::from_secs(3);

/// Coalesces rapid hot-plug edges so a service is not restarted again while a
/// prior restart for the same device class is still settling.
///
/// The supervisor drives this serially: each edge calls `should_restart`, and
/// only when it returns true does the restart run. A restart that was just
/// issued marks the device class as restarted, and any further edge inside the
/// debounce window is coalesced. A coalesced edge is not lost: it leaves a
/// trailing restart pending, which [`HotplugCoordinator::due_trailing`] hands
/// back once the window has passed. A remove-then-add inside the window thus
/// ends with one restart AFTER the device returned, not only the one issued
/// while it was absent.
#[derive(Debug, Default)]
pub struct HotplugCoordinator {
    last_restart: HashMap<DevKind, Instant>,
    trailing: HashSet<DevKind>,
    debounce: Option<Duration>,
}

impl HotplugCoordinator {
    /// Coordinator using the standard debounce window.
    pub fn new() -> Self {
        Self::with_debounce(HOTPLUG_DEBOUNCE)
    }

    /// Coordinator with an explicit debounce window (testable).
    pub fn with_debounce(debounce: Duration) -> Self {
        HotplugCoordinator {
            last_restart: HashMap::new(),
            trailing: HashSet::new(),
            debounce: Some(debounce),
        }
    }

    /// Decide whether a hot-plug edge for `kind` at `now` should issue a
    /// restart. Returns true (and records `now` as the restart time) only when
    /// no restart for the same class landed inside the debounce window;
    /// otherwise the edge is coalesced, a trailing restart is left pending,
    /// and false is returned.
    pub fn should_restart(&mut self, kind: DevKind, now: Instant) -> bool {
        if let Some(window) = self.debounce {
            if let Some(&last) = self.last_restart.get(&kind) {
                if now.duration_since(last) < window {
                    self.trailing.insert(kind);
                    return false;
                }
            }
        }
        self.last_restart.insert(kind, now);
        self.trailing.remove(&kind);
        true
    }

    /// The device classes whose coalesced edge is now due its trailing
    /// restart. Each returned class is recorded as restarted at `now`.
    pub fn due_trailing(&mut self, now: Instant) -> Vec<DevKind> {
        let window = self.debounce.unwrap_or_default();
        let due: Vec<DevKind> = self
            .trailing
            .iter()
            .copied()
            .filter(|k| {
                self.last_restart
                    .get(k)
                    .is_none_or(|&last| now.duration_since(last) >= window)
            })
            .collect();
        for k in &due {
            self.trailing.remove(k);
            self.last_restart.insert(*k, now);
        }
        due
    }
}

/// One identity per present device, per class.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Presence {
    camera: BTreeSet<String>,
    fc: BTreeSet<String>,
    radio: BTreeSet<String>,
    elrs: BTreeSet<String>,
}

/// Identities of every WFB radio on the bus, by the same generated adapter
/// table the boot detection uses ([`hardware::is_wfb_adapter_id`]). One table:
/// a second, hand-kept PID list here once left every TP-Link adapter the rest
/// of the stack supports invisible to hot-plug recovery.
fn radio_ids() -> BTreeSet<String> {
    radio_ids_in(&hardware::enumerate_usb_devices())
}

/// [`radio_ids`] over an explicit USB inventory. Pure for testing.
fn radio_ids_in(devices: &[hardware::UsbDevice]) -> BTreeSet<String> {
    devices
        .iter()
        .filter(|d| hardware::is_wfb_adapter_id(d.vid, d.pid))
        .map(hardware::UsbDevice::identity)
        .collect()
}

/// The `radio.crsf` claim from the agent config: the pinned RC-module device
/// and the lane mode (which owns the pinned port). Read fresh on every poll so
/// an operator pinning the device or flipping the mode through the config
/// surface reclassifies the node without a supervisor restart.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CrsfClaim {
    device: String,
    /// What the module carries (`radio.crsf.mode`), which decides who owns the
    /// pinned port: `crsf_rc` (the RC lane service owns it), `mavlink` (the
    /// MAVLink router owns it as its FC source), or `airport`. Defaults to
    /// `crsf_rc`, matching the config model, so a pinned device with no mode
    /// set is the RC module.
    mode: String,
}

impl Default for CrsfClaim {
    fn default() -> Self {
        Self {
            device: String::new(),
            mode: "crsf_rc".to_string(),
        }
    }
}

fn crsf_claim() -> CrsfClaim {
    let path =
        std::env::var("ADOS_CONFIG").unwrap_or_else(|_| crate::config::CONFIG_YAML.to_string());
    read_crsf_claim(Path::new(&path))
}

fn read_crsf_claim(path: &Path) -> CrsfClaim {
    #[derive(Default, Deserialize)]
    struct Raw {
        #[serde(default)]
        radio: RadioSection,
    }
    #[derive(Default, Deserialize)]
    struct RadioSection {
        #[serde(default)]
        crsf: CrsfSection,
    }
    #[derive(Default, Deserialize)]
    struct CrsfSection {
        // Nullable on disk (the config model writes `device: null` for "no
        // pin"); a bare String would fail the whole parse on the explicit null.
        #[serde(default)]
        device: Option<String>,
        // The lane mode decides who owns the pinned port: `mavlink` hands it
        // to the MAVLink router as its FC source, every other mode leaves it
        // to the RC lane. Nullable on disk like `device`.
        #[serde(default)]
        mode: Option<String>,
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return CrsfClaim::default();
    };
    // Quiet fallback by design: this is a SECONDARY, once-per-poll read of the
    // same file whose parse error the supervisor's own config load already
    // reports loudly (and publishes to the config-status sidecar) at startup.
    // An error!-per-poll here would flood the journal at 1 Hz.
    match serde_norway::from_str::<Raw>(&text) {
        Ok(raw) => CrsfClaim {
            device: raw
                .radio
                .crsf
                .device
                .as_deref()
                .unwrap_or("")
                .trim()
                .to_string(),
            mode: raw
                .radio
                .crsf
                .mode
                .as_deref()
                .unwrap_or("crsf_rc")
                .trim()
                .to_string(),
        },
        Err(e) => {
            tracing::debug!(error = %e, "hotplug config read failed; no crsf claim");
            CrsfClaim::default()
        }
    }
}

/// Resolve the configured pin to a bare device-node name (`ttyUSB0`): a
/// symlink pin (`/dev/serial/by-id/…`) canonicalizes to its target node, any
/// other spelling takes the path's basename verbatim. Empty pin -> empty name
/// (matches no node).
fn pin_node_name(pin: &str) -> String {
    let trimmed = pin.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let resolved = std::fs::canonicalize(trimmed).unwrap_or_else(|_| PathBuf::from(trimmed));
    resolved
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Split the USB-serial tty inventory into `(fc, elrs)` identity sets, keyed
/// on the pin and the lane mode — pin-only, mode-aware ownership that mirrors
/// the MAVLink router's. Only the PINNED node is ever the RC module, and only
/// when the router does not own it:
///
/// - **`mavlink` mode** hands the pinned module to the MAVLink router as its
///   FC source, so the pinned node counts as FC presence — a replug restarts
///   the FC link, which is what owns the carrier.
/// - **every other mode** (`crsf_rc`, `airport`) leaves the pinned port to the
///   RC lane service, so the pinned node counts as ELRS presence.
/// - **every non-pinned node** is an FC candidate, in any mode. The USB id is
///   NOT consulted: a VID:PID alone cannot distinguish an FC behind a
///   CP2102/CH340/ESP32-S3 bridge from an RC module behind the same one, and
///   the router excludes only the pinned node (never by bridge VID), so
///   claiming an unpinned bridge as ELRS would steal a bridge-connected FC.
///
/// The two classes are exclusive per node, so plugging the RC module never
/// changes `fc` (no spurious FC-service restart) and plugging an FC never
/// changes `elrs`.
fn classify_serial_nodes(
    nodes: &[hardware::SerialNode],
    pin_node: &str,
    mode: &str,
) -> (BTreeSet<String>, BTreeSet<String>) {
    // In `mavlink` mode the router owns the pinned module as its FC source; in
    // every other mode the RC lane owns/reserves the pinned port.
    let router_owns_pin = mode == "mavlink";
    let mut fc = BTreeSet::new();
    let mut elrs = BTreeSet::new();
    for node in nodes {
        let pinned = !pin_node.is_empty() && node.name == pin_node;
        if pinned && !router_owns_pin {
            elrs.insert(node.identity());
        } else {
            fc.insert(node.identity());
        }
    }
    (fc, elrs)
}

fn snapshot() -> Presence {
    let claim = crsf_claim();
    let pin_node = pin_node_name(&claim.device);
    // A USB flight controller enumerates as a CDC-ACM / USB-serial node — but
    // so does an ELRS RC module's bridge, so the tty inventory is classified
    // node-by-node instead of read as one class-wide presence.
    let (fc, elrs) = classify_serial_nodes(&hardware::serial_tty_nodes(), &pin_node, &claim.mode);
    Presence {
        camera: hardware::video_node_ids(),
        fc,
        radio: radio_ids(),
        elrs,
    }
}

/// Effective poll interval: stretch to 10s on a low-RAM board to avoid
/// swap-induced scheduler stalls, matching the Python monitor.
pub fn poll_interval() -> Duration {
    Duration::from_secs(if low_ram() { 10 } else { 1 })
}

/// MemTotal threshold (in MB) below which the hot-plug poller stretches its
/// interval to avoid swap-induced scheduler stalls. This is the poll-interval
/// concern only — it intentionally differs from the kiosk's separate minimal-UI
/// threshold (3 GiB), which governs a render-layer choice, not poll cadence.
/// Boards near or above this size keep their sysfs inodes warm in page cache,
/// so the 1s scan is cheap.
const HOTPLUG_LOW_RAM_MB: u64 = 1500;

fn low_ram() -> bool {
    // /proc/meminfo MemTotal is reported in kB.
    let Ok(text) = std::fs::read_to_string("/proc/meminfo") else {
        return false;
    };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            if let Some(kb) = rest.split_whitespace().next() {
                if let Ok(kb) = kb.parse::<u64>() {
                    return kb / 1024 < HOTPLUG_LOW_RAM_MB;
                }
            }
        }
    }
    false
}

/// Emit hot-plug events until the channel closes. The first snapshot is the
/// baseline; only subsequent changes to a class's identity set fire.
pub async fn run(tx: Sender<DevKind>, interval: Duration) {
    let mut prev = snapshot();
    tracing::info!(
        camera = prev.camera.len(),
        fc = prev.fc.len(),
        radio = prev.radio.len(),
        elrs = prev.elrs.len(),
        "hotplug baseline established"
    );
    loop {
        sleep(interval).await;
        let cur = snapshot();
        for (changed, kind) in [
            (cur.camera != prev.camera, DevKind::Camera),
            (cur.fc != prev.fc, DevKind::Fc),
            (cur.radio != prev.radio, DevKind::Radio),
            (cur.elrs != prev.elrs, DevKind::Elrs),
        ] {
            if changed && tx.send(kind).await.is_err() {
                return; // receiver gone → supervisor shutting down
            }
        }
        prev = cur;
    }
}

#[cfg(test)]
mod tests;
