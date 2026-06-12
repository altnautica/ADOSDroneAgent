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
use std::time::{Duration, Instant};

use tokio::sync::mpsc::Sender;
use tokio::time::sleep;

use crate::hardware;

/// Device classes the supervisor restarts a service for on a hot-plug change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DevKind {
    Camera,
    Fc,
    Radio,
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

fn snapshot() -> Presence {
    Presence {
        camera: hardware::video_node_present(),
        // A USB flight controller enumerates as a CDC-ACM / USB-serial node.
        fc: hardware::dev_nodes_present(&["ttyACM", "ttyUSB"]),
        radio: radio_present(),
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
        "hotplug baseline established"
    );
    loop {
        sleep(interval).await;
        let cur = snapshot();
        for (changed, kind) in [
            (cur.camera != prev.camera, DevKind::Camera),
            (cur.fc != prev.fc, DevKind::Fc),
            (cur.radio != prev.radio, DevKind::Radio),
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

        // Each class debounces only against its own last restart.
        assert!(!c.should_restart(DevKind::Fc, t0 + Duration::from_secs(1)));
        assert!(!c.should_restart(DevKind::Camera, t0 + Duration::from_secs(1)));
        assert!(!c.should_restart(DevKind::Radio, t0 + Duration::from_secs(1)));
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
