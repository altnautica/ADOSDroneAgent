//! Hot-plug detection for RTL8812-family WFB-ng dongles.
//!
//! Two backends:
//!
//! - [`SysfsUdev`] (default) polls `/sys/class/net/` and emits
//!   [`DongleEvent::Added`] / [`DongleEvent::Removed`] when an interface
//!   appears or disappears whose driver name carries an RTL8812 marker.
//!   Polling rather than `inotify` keeps the dependency footprint flat
//!   on a 256 MB SBC and avoids the gotcha that `inotify` does not fire
//!   on `/sys` (sysfs is a synthetic filesystem). The poll interval
//!   defaults to 1 s.
//! - [`MockUdev`] (behind the `mock` feature) lets tests inject events
//!   directly without touching `/sys`. Used by unit tests in this crate
//!   and by integration tests in downstream crates that exercise the
//!   `WfbManager` state machine.
//!
//! Both backends produce a `tokio::sync::mpsc::Receiver<DongleEvent>`
//! the manager can `.recv().await` on.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use thiserror::Error;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tracing::{debug, trace, warn};

/// Substring match against the `DRIVER=` field of `/sys/class/net/<iface>/device/uevent`.
/// Empirically the RTL8812EU dongles register either `8812` or `88XXau` as
/// their driver name depending on which out-of-tree driver flavor the
/// operator built. Both pass the same test.
const RTL8812_DRIVER_MARKERS: &[&str] = &["8812", "88XXau", "88xxau"];

/// Default poll cadence for the sysfs backend.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Channel depth between the watcher task and the manager. Two events
/// in flight is plenty: the manager processes events well under a
/// second and the only reason to buffer at all is to ride out a
/// hot-unplug-immediately-replug burst.
const EVENT_CHANNEL_DEPTH: usize = 8;

/// Hot-plug event surfaced to the WFB manager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DongleEvent {
    /// An interface matching the RTL8812 driver heuristic appeared.
    Added(String),
    /// An interface previously seen has gone away.
    Removed(String),
}

/// Errors raised by the sysfs udev path.
#[derive(Debug, Error)]
pub enum UdevError {
    /// `/sys/class/net` was not readable. On a real Linux host this is
    /// fatal; on macOS dev hosts the test path uses [`MockUdev`] instead.
    #[error("sysfs net root not accessible at {path}: {source}")]
    SysfsUnreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Filter that decides whether a candidate interface is an RTL8812-family
/// dongle. Returns the driver string that matched on success so callers
/// can log which marker fired.
pub fn is_rtl8812_driver(uevent_text: &str) -> Option<&str> {
    for line in uevent_text.lines() {
        if let Some(rest) = line.strip_prefix("DRIVER=") {
            for marker in RTL8812_DRIVER_MARKERS {
                if rest.contains(marker) {
                    return Some(marker);
                }
            }
        }
    }
    None
}

/// Read the driver string for an interface from `<sysfs>/<iface>/device/uevent`.
/// Returns `None` on any I/O error or when no `DRIVER=` line is present.
fn read_iface_driver_marker(sysfs_root: &Path, iface: &str) -> Option<&'static str> {
    let path = sysfs_root.join(iface).join("device").join("uevent");
    let text = std::fs::read_to_string(&path).ok()?;
    let marker = is_rtl8812_driver(&text)?;
    // Resolve the borrowed marker back into a 'static slice so the
    // return type does not have to plumb a lifetime through the
    // watcher loop.
    RTL8812_DRIVER_MARKERS
        .iter()
        .copied()
        .find(|m| *m == marker)
}

/// Sysfs-backed hot-plug watcher. Construct via [`SysfsUdev::new`] for
/// the production path, or [`SysfsUdev::with_root`] for tests that point
/// at a `tempfile::TempDir`.
pub struct SysfsUdev {
    root: PathBuf,
    poll: Duration,
}

impl SysfsUdev {
    /// Watch the live `/sys/class/net` tree.
    pub fn new() -> Self {
        Self::with_root(PathBuf::from("/sys/class/net"))
    }

    /// Watch a custom root. Test path.
    pub fn with_root(root: PathBuf) -> Self {
        Self {
            root,
            poll: DEFAULT_POLL_INTERVAL,
        }
    }

    /// Override the poll cadence. Most tests want a much shorter
    /// interval than the production 1 s default.
    pub fn with_poll_interval(mut self, poll: Duration) -> Self {
        self.poll = poll;
        self
    }

    /// Spawn the watcher task and return the receiver end of the event
    /// stream. The task lives until `rx` is dropped.
    pub fn spawn(self) -> Result<mpsc::Receiver<DongleEvent>, UdevError> {
        // Probe sysfs root once at spawn time so the caller sees a
        // typed error instead of a watcher that silently emits nothing.
        if let Err(e) = std::fs::read_dir(&self.root) {
            return Err(UdevError::SysfsUnreadable {
                path: self.root.clone(),
                source: e,
            });
        }

        let (tx, rx) = mpsc::channel(EVENT_CHANNEL_DEPTH);
        let root = self.root;
        let poll = self.poll;
        tokio::spawn(async move {
            run_sysfs_watcher(root, poll, tx).await;
        });
        Ok(rx)
    }
}

impl Default for SysfsUdev {
    fn default() -> Self {
        Self::new()
    }
}

async fn run_sysfs_watcher(root: PathBuf, poll: Duration, tx: mpsc::Sender<DongleEvent>) {
    // Snapshot of currently-known dongle interfaces, keyed by iface
    // name with the driver marker as value (only used for logging).
    let mut known: HashMap<String, &'static str> = HashMap::new();

    loop {
        match scan_dongles(&root) {
            Ok(present) => {
                // Compute additions.
                for (iface, marker) in &present {
                    if !known.contains_key(iface) {
                        debug!(iface, marker, "wfb dongle appeared");
                        if tx.send(DongleEvent::Added(iface.clone())).await.is_err() {
                            return;
                        }
                    }
                }
                // Compute removals.
                let removed: Vec<String> = known
                    .keys()
                    .filter(|iface| !present.contains_key(iface.as_str()))
                    .cloned()
                    .collect();
                for iface in removed {
                    debug!(iface, "wfb dongle disappeared");
                    if tx.send(DongleEvent::Removed(iface)).await.is_err() {
                        return;
                    }
                }
                known = present;
            }
            Err(e) => {
                // Don't kill the watcher on a transient read error; sysfs
                // can race with hot-plug events.
                warn!(error = %e, "wfb dongle scan failed, retrying");
            }
        }

        sleep(poll).await;
        trace!("wfb dongle scan tick");
    }
}

fn scan_dongles(root: &Path) -> std::io::Result<HashMap<String, &'static str>> {
    let mut out = HashMap::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let name = match entry.file_name().into_string() {
            Ok(n) => n,
            Err(_) => continue,
        };
        if !name.starts_with("wlan") {
            continue;
        }
        if let Some(marker) = read_iface_driver_marker(root, &name) {
            out.insert(name, marker);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// MockUdev — test-only event injector.
// ---------------------------------------------------------------------------

/// Test-only mock that hands out a receiver and lets the test push
/// events into it. Hidden behind the `mock` feature so production
/// builds do not pay any code-size cost.
#[cfg(any(test, feature = "mock"))]
pub struct MockUdev {
    tx: mpsc::Sender<DongleEvent>,
}

#[cfg(any(test, feature = "mock"))]
impl MockUdev {
    /// Construct a paired sender/receiver. The test holds `MockUdev` to
    /// push events; the manager holds the `Receiver`.
    pub fn new() -> (Self, mpsc::Receiver<DongleEvent>) {
        let (tx, rx) = mpsc::channel(EVENT_CHANNEL_DEPTH);
        (Self { tx }, rx)
    }

    /// Push a synthetic add event.
    pub async fn add(&self, iface: &str) {
        let _ = self.tx.send(DongleEvent::Added(iface.to_string())).await;
    }

    /// Push a synthetic remove event.
    pub async fn remove(&self, iface: &str) {
        let _ = self.tx.send(DongleEvent::Removed(iface.to_string())).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// MockUdev `Added` events surface on the receiver.
    #[tokio::test]
    async fn mock_udev_add_event_propagates() {
        let (mock, mut rx) = MockUdev::new();
        mock.add("wlan0").await;
        let evt = rx.recv().await.expect("event");
        assert_eq!(evt, DongleEvent::Added("wlan0".to_string()));
    }

    /// MockUdev `Removed` events surface on the receiver.
    #[tokio::test]
    async fn mock_udev_remove_event_propagates() {
        let (mock, mut rx) = MockUdev::new();
        mock.remove("wlan1").await;
        let evt = rx.recv().await.expect("event");
        assert_eq!(evt, DongleEvent::Removed("wlan1".to_string()));
    }

    /// A generic Wi-Fi adapter (e.g., a Cypress chip on a Pi) must NOT
    /// pass the RTL8812 filter. The filter exists precisely so the
    /// manager doesn't try to fire `wfb_tx` against the on-board Wi-Fi
    /// chip the operator is also using for the setup AP.
    #[test]
    fn dongle_filter_only_passes_rtl8812eu() {
        let cypress = "DEVTYPE=usb_interface\nDRIVER=brcmfmac\n";
        assert!(is_rtl8812_driver(cypress).is_none());

        let rtl_au = "DEVTYPE=usb_interface\nDRIVER=88XXau\n";
        assert!(is_rtl8812_driver(rtl_au).is_some());

        let rtl_eu = "DEVTYPE=usb_interface\nDRIVER=8812eu\n";
        assert!(is_rtl8812_driver(rtl_eu).is_some());

        let lower = "DRIVER=88xxau\n";
        assert!(is_rtl8812_driver(lower).is_some());
    }

    /// End-to-end with a tempdir as the sysfs root: when an interface
    /// appears with the right driver marker, the watcher emits an
    /// `Added` event.
    #[tokio::test]
    async fn sysfs_watcher_detects_added_interface() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();

        // Create a fake interface BEFORE spawning so the first scan
        // emits the event and we don't race the poll cadence.
        let iface_dir = root.join("wlan9").join("device");
        std::fs::create_dir_all(&iface_dir).expect("create iface");
        std::fs::write(iface_dir.join("uevent"), "DRIVER=88XXau\n").expect("write uevent");

        let mut rx = SysfsUdev::with_root(root)
            .with_poll_interval(Duration::from_millis(20))
            .spawn()
            .expect("spawn watcher");

        let evt = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("watcher emitted within timeout")
            .expect("event present");
        assert_eq!(evt, DongleEvent::Added("wlan9".to_string()));
    }

    /// Spawning against a non-existent sysfs root yields a typed error
    /// rather than a silently-dead watcher.
    #[test]
    fn sysfs_watcher_errors_on_missing_root() {
        let result = SysfsUdev::with_root(PathBuf::from("/nonexistent/sysfs/root")).spawn();
        match result {
            Err(UdevError::SysfsUnreadable { .. }) => {}
            other => panic!("expected SysfsUnreadable, got {other:?}"),
        }
    }
}
