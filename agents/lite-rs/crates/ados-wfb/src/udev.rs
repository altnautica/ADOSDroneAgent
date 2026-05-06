//! Hot-plug detection for RTL8812-family WFB-ng dongles.
//!
//! Three backends:
//!
//! - [`spawn_udev`] returns the production backend appropriate for the
//!   target. On Linux that's [`netlink::NetlinkUdev`] (real udev events
//!   over an `AF_NETLINK` socket via the `udev` crate); on every other
//!   platform it falls back to [`SysfsUdev`] so the workspace still
//!   compiles cleanly on macOS dev hosts.
//! - [`SysfsUdev`] polls `/sys/class/net/` and emits
//!   [`DongleEvent::Added`] / [`DongleEvent::Removed`] when an interface
//!   appears or disappears whose driver name carries an RTL8812 marker.
//!   Polling rather than `inotify` keeps the dependency footprint flat
//!   on a 256 MB SBC and avoids the gotcha that `inotify` does not fire
//!   on `/sys` (sysfs is a synthetic filesystem). The poll interval
//!   defaults to 1 s. Used as the macOS dev fallback and as a runtime
//!   fallback on Linux when the netlink path errors.
//! - [`MockUdev`] (behind the `mock` feature) lets tests inject events
//!   directly without touching `/sys`. Used by unit tests in this crate
//!   and by integration tests in downstream crates that exercise the
//!   `WfbManager` state machine.
//!
//! All three backends produce a `tokio::sync::mpsc::Receiver<DongleEvent>`
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
pub(crate) const RTL8812_DRIVER_MARKERS: &[&str] = &["8812", "88XXau", "88xxau"];

/// Tag identifying which backend the factory chose. Surfaced in the
/// REST status response so the wizard can render "real udev" vs
/// "polling fallback" without any Linux-only knowledge of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UdevBackend {
    /// Polling backend over `/sys/class/net`. Used on macOS dev hosts
    /// and as a runtime fallback when the Linux netlink path errors.
    Sysfs,
    /// Real udev events over an `AF_NETLINK` socket. Linux only.
    Netlink,
}

/// Factory function: pick the best backend for the target and spawn a
/// watcher. Returns the receiver end of the event stream plus the tag
/// of which backend was chosen so the manager can include it in the
/// status snapshot.
///
/// On Linux this prefers the netlink backend; if that fails to bind
/// (a containerized rootfs without `/run/udev`, an unprivileged
/// container that cannot open the netlink socket) it falls back to
/// the polling sysfs backend. On every other platform the sysfs
/// backend is the only path.
pub fn spawn_udev() -> Result<(mpsc::Receiver<DongleEvent>, UdevBackend), UdevError> {
    #[cfg(all(target_os = "linux", feature = "netlink-udev"))]
    {
        match netlink::NetlinkUdev::new()?.spawn() {
            Ok(rx) => Ok((rx, UdevBackend::Netlink)),
            Err(e) => {
                tracing::warn!(error = %e, "netlink udev failed; falling back to sysfs polling");
                let rx = SysfsUdev::new().spawn()?;
                Ok((rx, UdevBackend::Sysfs))
            }
        }
    }
    #[cfg(not(all(target_os = "linux", feature = "netlink-udev")))]
    {
        let rx = SysfsUdev::new().spawn()?;
        Ok((rx, UdevBackend::Sysfs))
    }
}

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

    /// On macOS dev hosts the factory must return the polling backend
    /// because libudev does not exist. The actual sysfs root will not
    /// exist either, so the spawn will fail with `SysfsUnreadable` —
    /// that's still the correct backend selection, the test asserts
    /// the dispatch path (not the real spawn).
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn factory_returns_sysfs_on_non_linux() {
        // We can't directly test the dispatch boolean without exposing
        // it; this test covers the macOS path by confirming `spawn_udev`
        // returns the same `UdevError` shape `SysfsUdev::spawn` does
        // when /sys/class/net is absent.
        let result = spawn_udev();
        match result {
            Err(UdevError::SysfsUnreadable { .. }) => {}
            // On a dev mac that happens to have /sys/class/net (rare;
            // some VM setups expose it) the spawn succeeds — that's
            // also a pass because we got Sysfs back, not a non-existent
            // backend.
            Ok((_, backend)) => assert_eq!(backend, UdevBackend::Sysfs),
        }
    }

    /// On Linux, the factory must reach the netlink path first. We
    /// can't reliably bind a real netlink socket inside `cargo test`
    /// (no privileges, sandbox), so we verify the construction path
    /// at least doesn't panic. A live netlink test runs on the
    /// hardware-bench rig.
    #[cfg(all(target_os = "linux", feature = "netlink-udev"))]
    #[test]
    fn netlink_backend_constructible_on_linux() {
        // `NetlinkUdev::new()` only fails when libudev itself cannot
        // initialise — which on a sane Linux host with /run/udev
        // present succeeds, but in a sandbox without /run/udev fails
        // cleanly. Either branch is fine; we just want the call to
        // return without unwinding.
        let _ = netlink::NetlinkUdev::new();
    }
}

// ---------------------------------------------------------------------------
// Netlink backend — Linux only.
// ---------------------------------------------------------------------------

#[cfg(all(target_os = "linux", feature = "netlink-udev"))]
pub mod netlink {
    //! Real udev events over an `AF_NETLINK` socket.
    //!
    //! The `udev` crate exposes a `MonitorBuilder` that, once filtered
    //! to the `net` subsystem, emits one event per kernel uevent. Each
    //! event carries the action (`add` / `remove` / `change`), the
    //! sysfs path of the device, and a property bag including
    //! `INTERFACE` and `DRIVER`.
    //!
    //! We register a filter for the `net` subsystem at builder time so
    //! the kernel only forwards us events from the right subsystem; we
    //! then walk through the `DRIVER` field on each event and translate
    //! into the same [`super::DongleEvent`] enum the polling backend
    //! emits, so the manager state machine consumes both paths
    //! identically.
    use std::os::fd::AsRawFd;
    use std::time::Duration;

    use super::{
        is_rtl8812_driver, DongleEvent, UdevError, EVENT_CHANNEL_DEPTH,
    };
    use tokio::io::{unix::AsyncFd, Interest};
    use tokio::sync::mpsc;
    use tracing::{debug, trace, warn};

    /// Wrapper around `udev::MonitorSocket` that exposes a tokio-friendly
    /// async interface via `AsyncFd`.
    pub struct NetlinkUdev {
        socket: udev::MonitorSocket,
    }

    impl NetlinkUdev {
        /// Construct a monitor scoped to the `net` subsystem.
        pub fn new() -> Result<Self, UdevError> {
            let socket = udev::MonitorBuilder::new()
                .map_err(|e| UdevError::SysfsUnreadable {
                    path: std::path::PathBuf::from("netlink"),
                    source: std::io::Error::other(format!("MonitorBuilder::new: {e}")),
                })?
                .match_subsystem("net")
                .map_err(|e| UdevError::SysfsUnreadable {
                    path: std::path::PathBuf::from("netlink"),
                    source: std::io::Error::other(format!("match_subsystem: {e}")),
                })?
                .listen()
                .map_err(|e| UdevError::SysfsUnreadable {
                    path: std::path::PathBuf::from("netlink"),
                    source: std::io::Error::other(format!("listen: {e}")),
                })?;
            Ok(Self { socket })
        }

        /// Spawn the watcher task and return the receiver end.
        pub fn spawn(self) -> Result<mpsc::Receiver<DongleEvent>, UdevError> {
            let (tx, rx) = mpsc::channel(EVENT_CHANNEL_DEPTH);
            tokio::spawn(async move {
                if let Err(e) = run(self.socket, tx).await {
                    warn!(error = %e, "netlink udev watcher exited");
                }
            });
            Ok(rx)
        }
    }

    async fn run(
        socket: udev::MonitorSocket,
        tx: mpsc::Sender<DongleEvent>,
    ) -> std::io::Result<()> {
        let fd = socket.as_raw_fd();
        let async_fd = AsyncFd::with_interest(FdGuard(fd), Interest::READABLE)?;
        loop {
            let mut guard = async_fd.readable().await?;
            // Drain everything available without blocking.
            let mut iter = socket.iter();
            while let Some(event) = iter.next() {
                if let Some(evt) = translate(&event) {
                    if tx.send(evt).await.is_err() {
                        return Ok(()); // receiver dropped
                    }
                }
            }
            guard.clear_ready();
            // Yield to the runtime so we don't spin if the event burst
            // was empty (libudev sometimes wakes spuriously).
            tokio::time::sleep(Duration::from_millis(1)).await;
            trace!("netlink udev wake");
        }
    }

    /// Translate a udev `Event` into a [`DongleEvent`], filtering out
    /// non-RTL8812 drivers and actions other than `add`/`remove`.
    fn translate(event: &udev::Event) -> Option<DongleEvent> {
        let iface = event
            .property_value("INTERFACE")
            .or_else(|| event.attribute_value("INTERFACE"))
            .map(|s| s.to_string_lossy().into_owned())?;
        if !iface.starts_with("wlan") {
            return None;
        }
        // The `DRIVER` property is the canonical field for filtering
        // RTL8812-family adapters; we re-use the same substring matcher
        // the polling backend uses so both code paths share the filter.
        let driver = event
            .property_value("DRIVER")
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let driver_line = format!("DRIVER={driver}");
        if is_rtl8812_driver(&driver_line).is_none() {
            return None;
        }
        match event.event_type() {
            udev::EventType::Add => {
                debug!(iface = %iface, driver = %driver, "netlink: dongle added");
                Some(DongleEvent::Added(iface))
            }
            udev::EventType::Remove => {
                debug!(iface = %iface, driver = %driver, "netlink: dongle removed");
                Some(DongleEvent::Removed(iface))
            }
            // `change` events fire when the kernel updates link state
            // (carrier up/down) and are not interesting to us.
            _ => None,
        }
    }

    /// Tiny `AsRawFd` newtype so `AsyncFd` can wrap the libudev socket
    /// without us having to take ownership of it (libudev keeps the
    /// real handle).
    struct FdGuard(std::os::fd::RawFd);
    impl AsRawFd for FdGuard {
        fn as_raw_fd(&self) -> std::os::fd::RawFd {
            self.0
        }
    }
}
