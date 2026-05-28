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

use std::time::Duration;

use tokio::sync::mpsc::Sender;
use tokio::time::sleep;

use crate::hardware;

/// Device classes the supervisor restarts a service for on a hot-plug change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevKind {
    Camera,
    Fc,
    Radio,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Presence {
    camera: bool,
    fc: bool,
    radio: bool,
}

/// RTL8812-family PIDs the hot-plug path treats as the WFB radio (the wider
/// set the Python hot-plug router matched, a superset of the boot-detect set).
const RTL_PIDS: [u16; 5] = [0x8812, 0x881A, 0x881B, 0x881C, 0xB812];
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

fn low_ram() -> bool {
    // /proc/meminfo MemTotal in kB; < ~1.5 GB is the low-RAM tier.
    let Ok(text) = std::fs::read_to_string("/proc/meminfo") else {
        return false;
    };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            if let Some(kb) = rest.split_whitespace().next() {
                if let Ok(kb) = kb.parse::<u64>() {
                    return kb / 1024 < 1500;
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
