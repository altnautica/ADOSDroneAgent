//! Input-device lifecycle for the ground-station profile.
//!
//! Ports `src/ados/services/ground_station/input_manager.py`: USB gamepad
//! enumeration via evdev, the 1 Hz hotplug poll of `/dev/input`, the
//! gamepad-detection predicate, stable device-id formatting, and primary-device
//! persistence to `/etc/ados/ground-station-input.json` (reusing the chunk-1
//! [`crate::sidecar::GroundStationInput`] helper).
//!
//! The diff engine ([`HotplugTracker`]), the gamepad predicate, and the
//! device-id formatter are PURE and host-portable, unit-testable with
//! synthetic snapshots. Only the evdev enumeration is target-gated to Linux.
//!
//! Bluetooth (bluetoothctl scan/pair/forget) stays in the Python service; it is
//! a thin subprocess wrapper with no shared state and is deferred rather than
//! reimplemented here.

use std::collections::BTreeMap;
use std::path::Path;

use crate::sidecar::GroundStationInput;

/// Hotplug poll cadence.
pub const HOTPLUG_POLL_SECONDS: f64 = 1.0;

/// A real gamepad has two analog axes and a healthy button set. Keyboards/mice
/// fall below this button floor or miss the absolute axes.
pub const MIN_GAMEPAD_BUTTONS: usize = 8;

/// One attached gamepad. Field set mirrors the Python enumeration dict that the
/// REST layer and the hotplug watcher both consume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gamepad {
    pub device_id: String,
    pub name: String,
    pub path: String,
    pub vendor: u16,
    pub product: u16,
}

/// The kind of a hotplug transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotplugKind {
    Connected,
    Disconnected,
}

impl HotplugKind {
    pub fn as_str(self) -> &'static str {
        match self {
            HotplugKind::Connected => "connected",
            HotplugKind::Disconnected => "disconnected",
        }
    }
}

/// One hotplug observation produced by the diff engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotplugEvent {
    pub device_id: String,
    pub kind: HotplugKind,
    pub name: String,
    /// evdev node path on connect; empty on disconnect (the node is gone).
    pub path: String,
}

/// Stable USB device id: `usb:<vendor>:<product>:<node-basename>`. Mirrors the
/// Python `_device_id_for_usb` (lowercase 4-hex vendor/product, node basename).
pub fn device_id_for_usb(vendor: u16, product: u16, path: &str) -> String {
    let base = path
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("unknown");
    format!("usb:{vendor:04x}:{product:04x}:{base}")
}

/// Whether a device's capabilities make it a gamepad: both ABS_X and ABS_Y
/// present AND at least [`MIN_GAMEPAD_BUTTONS`] key codes. Pure predicate over
/// the capability summary so it is testable without evdev.
pub fn is_gamepad(has_abs_x: bool, has_abs_y: bool, key_code_count: usize) -> bool {
    has_abs_x && has_abs_y && key_code_count >= MIN_GAMEPAD_BUTTONS
}

/// A snapshot of the attached gamepad set, keyed by device id. A `BTreeMap`
/// keeps the "first device" deterministic for primary auto-promotion (the
/// Python `next(iter(...))` over an insertion-ordered dict is replaced by the
/// lowest device id, which is stable across polls).
pub type Snapshot = BTreeMap<String, Gamepad>;

/// What one poll produced: the transition events and whether a primary was
/// auto-promoted (and to which device).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PollOutcome {
    pub events: Vec<HotplugEvent>,
    pub auto_primary: Option<String>,
}

/// Pure hotplug diff engine. Holds the last-seen snapshot and the current
/// primary; each `poll` diffs a fresh snapshot against the last and reports
/// connect/disconnect events. The first poll seeds without emitting (devices
/// already attached at start are not re-announced), matching the Python
/// `first_pass`. When no primary is set and a gamepad is present, the lowest
/// device id is auto-promoted.
#[derive(Debug, Default)]
pub struct HotplugTracker {
    last_seen: Snapshot,
    primary: Option<String>,
    first_pass: bool,
}

impl HotplugTracker {
    /// A tracker seeded with the persisted primary (if any). `first_pass` is set
    /// so the initial poll only seeds the snapshot.
    pub fn new(primary: Option<String>) -> Self {
        Self {
            last_seen: Snapshot::new(),
            primary,
            first_pass: true,
        }
    }

    /// Load the persisted primary from the sidecar and build a tracker.
    pub fn from_sidecar(path: &Path) -> Self {
        let primary = GroundStationInput::load(path).and_then(|g| g.primary);
        Self::new(primary)
    }

    pub fn primary(&self) -> Option<&str> {
        self.primary.as_deref()
    }

    /// Diff `snapshot` against the last-seen set. On the first poll, only seeds.
    /// Returns the transition events plus any auto-promoted primary.
    pub fn poll(&mut self, snapshot: Snapshot) -> PollOutcome {
        let mut outcome = PollOutcome::default();

        if !self.first_pass {
            // Connects: in the new set, not in the old.
            for (id, dev) in &snapshot {
                if !self.last_seen.contains_key(id) {
                    outcome.events.push(HotplugEvent {
                        device_id: id.clone(),
                        kind: HotplugKind::Connected,
                        name: dev.name.clone(),
                        path: dev.path.clone(),
                    });
                }
            }
            // Disconnects: in the old set, not in the new.
            for (id, dev) in &self.last_seen {
                if !snapshot.contains_key(id) {
                    outcome.events.push(HotplugEvent {
                        device_id: id.clone(),
                        kind: HotplugKind::Disconnected,
                        name: dev.name.clone(),
                        path: String::new(),
                    });
                }
            }
        }

        // Auto-promote the lowest device id to primary when none is set.
        if self.primary.is_none() {
            if let Some((id, _)) = snapshot.iter().next() {
                self.primary = Some(id.clone());
                outcome.auto_primary = Some(id.clone());
            }
        }

        self.last_seen = snapshot;
        self.first_pass = false;
        outcome
    }

    /// Persist the current primary to the sidecar. Best-effort; the caller logs
    /// an error and continues on failure.
    pub fn save_primary(&self, path: &Path) -> std::io::Result<()> {
        let blob = GroundStationInput {
            primary: self.primary.clone(),
        };
        blob.save(path)
    }
}

/// Enumerate attached USB gamepads via evdev. Returns an empty set when evdev
/// cannot list devices. The node-open + capability read is the only
/// hardware-coupled step; the gamepad predicate stays in [`is_gamepad`].
#[cfg(target_os = "linux")]
pub fn enumerate_gamepads() -> Snapshot {
    let mut snap = Snapshot::new();
    for (path, dev) in evdev::enumerate() {
        let path = path.to_string_lossy().to_string();
        if let Some(g) = gamepad_from_device(&dev, &path, |d, axis| {
            d.supported_absolute_axes()
                .map(|set| set.contains(axis))
                .unwrap_or(false)
        }) {
            snap.insert(g.device_id.clone(), g);
        }
    }
    snap
}

/// Build a [`Gamepad`] from an evdev device if it passes [`is_gamepad`]. The
/// axis probe is injected so the call site keeps the device-borrow rules simple.
#[cfg(target_os = "linux")]
fn gamepad_from_device<P>(dev: &evdev::Device, path: &str, has_axis: P) -> Option<Gamepad>
where
    P: Fn(&evdev::Device, evdev::AbsoluteAxisType) -> bool,
{
    use evdev::AbsoluteAxisType;

    let has_abs_x = has_axis(dev, AbsoluteAxisType::ABS_X);
    let has_abs_y = has_axis(dev, AbsoluteAxisType::ABS_Y);
    let key_count = dev.supported_keys().map(|k| k.iter().count()).unwrap_or(0);
    if !is_gamepad(has_abs_x, has_abs_y, key_count) {
        return None;
    }
    let id = dev.input_id();
    let vendor = id.vendor();
    let product = id.product();
    let name = dev.name().unwrap_or("unknown").to_string();
    Some(Gamepad {
        device_id: device_id_for_usb(vendor, product, path),
        name,
        path: path.to_string(),
        vendor,
        product,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pad(id: &str) -> Gamepad {
        Gamepad {
            device_id: id.to_string(),
            name: format!("pad-{id}"),
            path: format!("/dev/input/{id}"),
            vendor: 0x045e,
            product: 0x028e,
        }
    }

    fn snap(ids: &[&str]) -> Snapshot {
        ids.iter().map(|id| (id.to_string(), pad(id))).collect()
    }

    #[test]
    fn device_id_format_matches_python() {
        assert_eq!(
            device_id_for_usb(0x045e, 0x028e, "/dev/input/event3"),
            "usb:045e:028e:event3"
        );
        // Empty path falls back to "unknown".
        assert_eq!(device_id_for_usb(1, 2, ""), "usb:0001:0002:unknown");
    }

    #[test]
    fn gamepad_predicate_needs_both_axes_and_enough_buttons() {
        assert!(is_gamepad(true, true, 8));
        assert!(is_gamepad(true, true, 20));
        // Missing an axis.
        assert!(!is_gamepad(true, false, 20));
        assert!(!is_gamepad(false, true, 20));
        // Too few buttons (a touchpad / keyboard).
        assert!(!is_gamepad(true, true, 7));
    }

    #[test]
    fn first_poll_seeds_without_events_but_can_auto_promote() {
        let mut t = HotplugTracker::new(None);
        let out = t.poll(snap(&["usb:1", "usb:2"]));
        // No connect events on the seeding pass.
        assert!(out.events.is_empty());
        // The lowest device id is auto-promoted.
        assert_eq!(out.auto_primary.as_deref(), Some("usb:1"));
        assert_eq!(t.primary(), Some("usb:1"));
    }

    #[test]
    fn second_poll_reports_connect_and_disconnect() {
        let mut t = HotplugTracker::new(Some("usb:1".to_string()));
        t.poll(snap(&["usb:1"])); // seed
        let out = t.poll(snap(&["usb:1", "usb:2"]));
        assert_eq!(out.events.len(), 1);
        assert_eq!(out.events[0].kind, HotplugKind::Connected);
        assert_eq!(out.events[0].device_id, "usb:2");
        assert_eq!(out.events[0].path, "/dev/input/usb:2");

        // Now unplug usb:1.
        let out = t.poll(snap(&["usb:2"]));
        assert_eq!(out.events.len(), 1);
        assert_eq!(out.events[0].kind, HotplugKind::Disconnected);
        assert_eq!(out.events[0].device_id, "usb:1");
        // Disconnect carries an empty path (node already gone).
        assert!(out.events[0].path.is_empty());
    }

    #[test]
    fn no_events_when_set_unchanged() {
        let mut t = HotplugTracker::new(Some("usb:1".to_string()));
        t.poll(snap(&["usb:1"])); // seed
        let out = t.poll(snap(&["usb:1"]));
        assert!(out.events.is_empty());
        assert!(out.auto_primary.is_none());
    }

    #[test]
    fn primary_already_set_is_not_auto_promoted() {
        let mut t = HotplugTracker::new(Some("usb:9".to_string()));
        let out = t.poll(snap(&["usb:1", "usb:2"]));
        assert!(out.auto_primary.is_none());
        assert_eq!(t.primary(), Some("usb:9"));
    }

    #[test]
    fn auto_promote_only_once_across_polls() {
        let mut t = HotplugTracker::new(None);
        let out = t.poll(snap(&["usb:5"]));
        assert_eq!(out.auto_primary.as_deref(), Some("usb:5"));
        // A later poll does not re-promote even when devices change.
        let out = t.poll(snap(&["usb:5", "usb:6"]));
        assert!(out.auto_primary.is_none());
        assert_eq!(t.primary(), Some("usb:5"));
    }

    #[test]
    fn primary_round_trips_through_the_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ground-station-input.json");
        let mut t = HotplugTracker::new(None);
        t.poll(snap(&["usb:1"]));
        t.save_primary(&path).unwrap();
        // A fresh tracker rehydrates the same primary and does not re-promote.
        let mut t2 = HotplugTracker::from_sidecar(&path);
        assert_eq!(t2.primary(), Some("usb:1"));
        let out = t2.poll(snap(&["usb:1", "usb:2"]));
        assert!(out.auto_primary.is_none());
    }

    #[test]
    fn from_sidecar_with_no_file_has_no_primary() {
        let t = HotplugTracker::from_sidecar(Path::new("/nonexistent/gs-input.json"));
        assert!(t.primary().is_none());
    }

    #[test]
    fn hotplug_kind_wire_strings() {
        assert_eq!(HotplugKind::Connected.as_str(), "connected");
        assert_eq!(HotplugKind::Disconnected.as_str(), "disconnected");
    }
}
