//! USB hot-plug detection by presence-transition polling.
//!
//! Polls a small set of device-class presence flags on an interval (1s on a
//! normal board, 10s on a low-RAM SBC, matching the Python monitor's swap
//! sensitivity tradeoff) and emits an event when a class appears or
//! disappears. The first snapshot is the baseline, so devices already present
//! at boot do not fire — the equivalent of the Python first-scan gate.
//!
//! A future optimization is an event-driven udev monitor; presence-transition
//! polling is the proven, testable parity baseline.

use std::collections::HashMap;
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
/// only when it returns true does the restart run. Two guards combine to give
/// the Python "3s per-device debounce + per-service restart coalescing"
/// behavior:
///
/// - A restart that was just issued marks the device class as restarted, and
///   any further edge inside the debounce window is dropped (coalesced into the
///   in-flight / just-completed restart).
/// - Because the loop is serial, an edge that arrives while a restart is
///   actually running is queued behind it and then evaluated against the
///   just-recorded restart time — so it, too, coalesces.
#[derive(Debug, Default)]
pub struct HotplugCoordinator {
    last_restart: HashMap<DevKind, Instant>,
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
            debounce: Some(debounce),
        }
    }

    /// Decide whether a hot-plug edge for `kind` at `now` should issue a
    /// restart. Returns true (and records `now` as the restart time) only when
    /// no restart for the same class landed inside the debounce window;
    /// otherwise the edge is coalesced and false is returned.
    pub fn should_restart(&mut self, kind: DevKind, now: Instant) -> bool {
        if let Some(window) = self.debounce {
            if let Some(&last) = self.last_restart.get(&kind) {
                if now.duration_since(last) < window {
                    return false;
                }
            }
        }
        self.last_restart.insert(kind, now);
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Presence {
    camera: bool,
    fc: bool,
    radio: bool,
    elrs: bool,
}

/// RTL8812-family PIDs the hot-plug path treats as the WFB radio (the wider
/// set the Python hot-plug router matched, a superset of the boot-detect set).
/// MUST include every PID the boot-detect set (`hardware::WFB_IDS`) and the
/// bootstrap probe (`profile_detect.PY`) match, or a hot-plug edge on an adapter
/// the agent otherwise recognizes silently does nothing. `0xA81A` is the
/// production RTL8812EU (the `0bda:a81a` shipped on the dev rigs); it was the
/// missing one — present in `WFB_IDS` but dropped here, so unplug/replug of the
/// primary adapter never triggered recovery.
const RTL_PIDS: [u16; 6] = [0xA81A, 0x8812, 0x881A, 0x881B, 0x881C, 0xB812];
const REALTEK_VID: u16 = 0x0BDA;

fn radio_present() -> bool {
    hardware::enumerate_usb_ids()
        .iter()
        .any(|&(v, p)| v == REALTEK_VID && RTL_PIDS.contains(&p))
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

/// Split the USB-serial tty inventory into `(fc, elrs)` presence, keyed on the
/// pin and the lane mode — pin-only, mode-aware ownership that mirrors the
/// MAVLink router's. Only the PINNED node is ever the RC module, and only when
/// the router does not own it:
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
/// flips `fc` (no spurious FC-service restart) and plugging an FC never flips
/// `elrs`.
fn classify_serial_nodes(
    nodes: &[(String, Option<(u16, u16)>)],
    pin_node: &str,
    mode: &str,
) -> (bool, bool) {
    // In `mavlink` mode the router owns the pinned module as its FC source; in
    // every other mode the RC lane owns/reserves the pinned port.
    let router_owns_pin = mode == "mavlink";
    let mut fc = false;
    let mut elrs = false;
    for (name, _usb) in nodes {
        let pinned = !pin_node.is_empty() && name == pin_node;
        if pinned && !router_owns_pin {
            elrs = true;
        } else {
            fc = true;
        }
    }
    (fc, elrs)
}

fn snapshot() -> Presence {
    let claim = crsf_claim();
    let pin_node = pin_node_name(&claim.device);
    // A USB flight controller enumerates as a CDC-ACM / USB-serial node — but
    // so does an ELRS RC module's bridge, so the tty inventory is classified
    // node-by-node instead of read as one class-wide presence bool.
    let (fc, elrs) = classify_serial_nodes(&hardware::serial_tty_nodes(), &pin_node, &claim.mode);
    Presence {
        camera: hardware::video_node_present(),
        fc,
        radio: radio_present(),
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
/// baseline; only subsequent transitions fire.
pub async fn run(tx: Sender<DevKind>, interval: Duration) {
    let mut prev = snapshot();
    tracing::info!(
        camera = prev.camera,
        fc = prev.fc,
        radio = prev.radio,
        elrs = prev.elrs,
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
mod tests {
    use super::*;

    #[test]
    fn rtl_pids_include_every_boot_detect_pid() {
        // The hot-plug radio match must be a SUPERSET of the boot-detect set
        // (hardware::has_wfb_adapter's WFB_IDS) and the bootstrap probe, or an
        // adapter the agent recognizes at boot is invisible to hot-plug
        // recovery. 0xA81A is the production RTL8812EU (0bda:a81a) that was the
        // dropped one. Pin the whole boot-detect PID set here.
        for pid in [0xA81A, 0x8812, 0x881A] {
            assert!(
                RTL_PIDS.contains(&pid),
                "RTL_PIDS must contain boot-detect PID {pid:#06x}"
            );
        }
    }

    /// Node names for the classification tests. CP2102 / CH340 / Espressif are
    /// the RC-bridge ids; STM native USB is the archetypal FC.
    fn node(name: &str, usb: Option<(u16, u16)>) -> (String, Option<(u16, u16)>) {
        (name.to_string(), usb)
    }

    #[test]
    fn a_pinned_node_in_crsf_rc_mode_classifies_as_elrs() {
        // The RC lane owns the pinned port in the default mode: the module
        // alone reads (fc=false, elrs=true), never as an FC.
        let nodes = [node("ttyUSB0", Some((0x10C4, 0xEA60)))];
        assert_eq!(
            classify_serial_nodes(&nodes, "ttyUSB0", "crsf_rc"),
            (false, true)
        );
        // With an FC beside it, both classes are present and independent.
        let both = [
            node("ttyACM0", Some((0x0483, 0x5740))),
            node("ttyUSB0", Some((0x10C4, 0xEA60))),
        ];
        assert_eq!(
            classify_serial_nodes(&both, "ttyUSB0", "crsf_rc"),
            (true, true)
        );
    }

    #[test]
    fn a_pinned_node_in_mavlink_mode_classifies_as_fc() {
        // In mavlink mode the MAVLink router owns the pinned module as its FC
        // source, so the pinned node counts as FC presence — a replug restarts
        // the FC link (the owner), never the RC lane.
        let nodes = [node("ttyUSB0", Some((0x10C4, 0xEA60)))];
        assert_eq!(
            classify_serial_nodes(&nodes, "ttyUSB0", "mavlink"),
            (true, false)
        );
        // An FC beside it: both are FC, so ELRS stays absent.
        let both = [
            node("ttyACM0", Some((0x0483, 0x5740))),
            node("ttyUSB0", Some((0x10C4, 0xEA60))),
        ];
        assert_eq!(
            classify_serial_nodes(&both, "ttyUSB0", "mavlink"),
            (true, false)
        );
    }

    #[test]
    fn a_pinned_node_in_airport_mode_stays_off_the_fc_link() {
        // Only mavlink mode hands the pin to the router; airport leaves it to
        // the RC lane, so the pinned node reads ELRS (never a spurious FC
        // restart, since the router excludes the pin outside mavlink mode).
        let nodes = [node("ttyUSB0", Some((0x10C4, 0xEA60)))];
        assert_eq!(
            classify_serial_nodes(&nodes, "ttyUSB0", "airport"),
            (false, true)
        );
    }

    #[test]
    fn a_bridge_connected_fc_is_not_stolen_by_a_different_pinned_elrs() {
        // The bridge-VID over-claim is gone: with an ELRS module pinned on one
        // node, an FC behind a CP2102/CH340/ESP32-S3 bridge on ANOTHER node is
        // still FC (not stolen by VID). Only the pinned node is ELRS.
        let nodes = [
            node("ttyUSB0", Some((0x10C4, 0xEA60))), // an FC behind a CP2102 bridge
            node("ttyUSB1", Some((0x1A86, 0x7523))), // the pinned ELRS module
        ];
        assert_eq!(
            classify_serial_nodes(&nodes, "ttyUSB1", "crsf_rc"),
            (true, true)
        );
    }

    #[test]
    fn an_unpinned_bridge_node_is_an_fc_candidate_in_every_mode() {
        // With no pin, a known RC-bridge node is an FC candidate regardless of
        // mode — a VID:PID cannot distinguish an FC behind the bridge from an
        // RC module behind the same one, and only a pin makes it ELRS.
        for usb in [(0x10C4, 0xEA60), (0x1A86, 0x7523), (0x303A, 0x1001)] {
            for mode in ["crsf_rc", "mavlink", "airport"] {
                let nodes = [node("ttyUSB0", Some(usb))];
                assert_eq!(
                    classify_serial_nodes(&nodes, "", mode),
                    (true, false),
                    "usb={usb:?} mode={mode}"
                );
            }
        }
    }

    #[test]
    fn an_fc_replug_reads_fc_presence_so_the_fc_link_restarts() {
        // A real FC on ttyACM0 with the RC module pinned on ttyUSB0 (RC mode):
        // the FC reads fc=true, so its remove/add edge emits DevKind::Fc and
        // restarts ados-mavlink — while the pinned module reads ELRS and never
        // steals that restart.
        let with_fc = [
            node("ttyACM0", Some((0x0483, 0x5740))),
            node("ttyUSB0", Some((0x10C4, 0xEA60))),
        ];
        assert_eq!(
            classify_serial_nodes(&with_fc, "ttyUSB0", "crsf_rc"),
            (true, true)
        );
        // The FC unplugged: fc drops to false (elrs still present) — a real
        // presence transition on the FC class, which run() turns into a
        // DevKind::Fc restart of the FC link.
        let without_fc = [node("ttyUSB0", Some((0x10C4, 0xEA60)))];
        assert_eq!(
            classify_serial_nodes(&without_fc, "ttyUSB0", "crsf_rc"),
            (false, true)
        );
    }

    #[test]
    fn pin_matches_only_its_own_node() {
        // A pin on ttyUSB1 leaves ttyUSB0 an FC candidate and reads no ELRS
        // while the pinned node is absent (truthful: not present).
        let nodes = [node("ttyUSB0", None)];
        assert_eq!(
            classify_serial_nodes(&nodes, "ttyUSB1", "crsf_rc"),
            (true, false)
        );
        // No nodes at all: neither class present.
        assert_eq!(
            classify_serial_nodes(&[], "ttyUSB0", "crsf_rc"),
            (false, false)
        );
    }

    #[test]
    fn pin_node_name_takes_the_basename_of_an_unresolvable_path() {
        assert_eq!(
            pin_node_name("/dev/nonexistent-tty-xyz"),
            "nonexistent-tty-xyz"
        );
        assert_eq!(pin_node_name("  "), "");
        assert_eq!(pin_node_name(""), "");
    }

    #[cfg(unix)]
    #[test]
    fn pin_node_name_resolves_a_symlink_pin_to_its_target_node() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("ttyUSB0");
        std::fs::write(&real, b"").unwrap();
        let link = dir.path().join("usb-elrs-module-if00");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(pin_node_name(link.to_str().unwrap()), "ttyUSB0");
    }

    #[test]
    fn crsf_claim_reads_the_radio_section_and_defaults_empty() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        // No mode set → the default crsf_rc (the RC lane owns the pin).
        std::fs::write(
            &cfg,
            "radio:\n  crsf:\n    enabled: true\n    device: \" /dev/ttyUSB0 \"\n",
        )
        .unwrap();
        assert_eq!(
            read_crsf_claim(&cfg),
            CrsfClaim {
                device: "/dev/ttyUSB0".to_string(),
                mode: "crsf_rc".to_string(),
            }
        );
        // An explicit mavlink mode is read through, so the router-owned pin is
        // classified as FC, not ELRS.
        let mav = dir.path().join("mavlink.yaml");
        std::fs::write(
            &mav,
            "radio:\n  crsf:\n    enabled: true\n    device: /dev/ttyUSB0\n    mode: mavlink\n",
        )
        .unwrap();
        assert_eq!(
            read_crsf_claim(&mav),
            CrsfClaim {
                device: "/dev/ttyUSB0".to_string(),
                mode: "mavlink".to_string(),
            }
        );
        // Missing file / missing section / malformed file all read the default
        // claim (no pin, crsf_rc).
        assert_eq!(
            read_crsf_claim(&dir.path().join("nope.yaml")),
            CrsfClaim::default()
        );
        let bare = dir.path().join("bare.yaml");
        std::fs::write(&bare, "agent:\n  profile: drone\n").unwrap();
        assert_eq!(read_crsf_claim(&bare), CrsfClaim::default());
        let bad = dir.path().join("bad.yaml");
        std::fs::write(&bad, ": not yaml [\n").unwrap();
        assert_eq!(read_crsf_claim(&bad), CrsfClaim::default());
        // The config model writes `device: null` for "no pin"; the claim must
        // read it as an unpinned lane at the default mode, not fail the parse.
        let nulled = dir.path().join("nulled.yaml");
        std::fs::write(
            &nulled,
            "radio:\n  crsf:\n    enabled: true\n    device: null\n",
        )
        .unwrap();
        assert_eq!(
            read_crsf_claim(&nulled),
            CrsfClaim {
                device: String::new(),
                mode: "crsf_rc".to_string(),
            }
        );
    }

    #[test]
    fn first_edge_restarts_repeats_within_window_coalesce() {
        let mut c = HotplugCoordinator::with_debounce(Duration::from_secs(3));
        let t0 = Instant::now();

        // First edge for a device class issues a restart.
        assert!(c.should_restart(DevKind::Fc, t0));

        // A re-enumeration edge ~1s later (DFU → flight) is coalesced.
        assert!(!c.should_restart(DevKind::Fc, t0 + Duration::from_secs(1)));
        // And another inside the window.
        assert!(!c.should_restart(DevKind::Fc, t0 + Duration::from_millis(2500)));

        // Past the window, a fresh plug event restarts again.
        assert!(c.should_restart(DevKind::Fc, t0 + Duration::from_millis(3001)));
    }

    #[test]
    fn debounce_is_independent_per_device_class() {
        let mut c = HotplugCoordinator::with_debounce(Duration::from_secs(3));
        let t0 = Instant::now();

        // A flight-controller edge does not debounce a camera edge.
        assert!(c.should_restart(DevKind::Fc, t0));
        assert!(c.should_restart(DevKind::Camera, t0));
        assert!(c.should_restart(DevKind::Radio, t0));
        assert!(c.should_restart(DevKind::Elrs, t0));

        // Each class debounces only against its own last restart.
        assert!(!c.should_restart(DevKind::Fc, t0 + Duration::from_secs(1)));
        assert!(!c.should_restart(DevKind::Camera, t0 + Duration::from_secs(1)));
        assert!(!c.should_restart(DevKind::Radio, t0 + Duration::from_secs(1)));
        assert!(!c.should_restart(DevKind::Elrs, t0 + Duration::from_secs(1)));
    }

    #[test]
    fn window_boundary_is_exclusive() {
        let mut c = HotplugCoordinator::with_debounce(Duration::from_secs(3));
        let t0 = Instant::now();
        assert!(c.should_restart(DevKind::Radio, t0));
        // Exactly at the window edge the next edge is allowed (>= window).
        assert!(c.should_restart(DevKind::Radio, t0 + Duration::from_secs(3)));
    }
}
