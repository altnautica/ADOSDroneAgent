//! A cheap fingerprint of the node's video devices, read from sysfs.
//!
//! Full camera discovery spawns a Python interpreter plus `v4l2-ctl` and
//! `rpicam-hello` probes and can take seconds. On a drone with no camera the
//! cold-start retry used to run it every 5 s for the life of the process. The
//! fingerprint lets the retry skip discovery while nothing changed: the sorted
//! `/sys/class/video4linux/*` entries with the target of each one's `device`
//! link, plus the sorted `/dev/media*` nodes. A camera arriving or leaving
//! changes it; a SIGUSR1 hotplug and a 60 s ceiling force discovery anyway.

use std::path::Path;
use std::time::Duration;

/// Real sysfs class directory for V4L2 devices.
pub const SYS_VIDEO4LINUX: &str = "/sys/class/video4linux";
/// Real device directory holding the `media*` controller nodes.
pub const DEV_DIR: &str = "/dev";
/// Longest a camera-less node goes without a full discovery.
pub const DISCOVERY_CEILING: Duration = Duration::from_secs(60);

/// The fingerprint over `sys_v4l` and `dev_dir` (injected so tests are
/// hermetic). An unreadable directory contributes nothing.
pub fn fingerprint_in(sys_v4l: &Path, dev_dir: &Path) -> String {
    let mut v4l: Vec<String> = std::fs::read_dir(sys_v4l)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let target = std::fs::read_link(e.path().join("device"))
                        .map(|t| t.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    format!("{name}={target}")
                })
                .collect()
        })
        .unwrap_or_default();
    v4l.sort();
    let mut media: Vec<String> = std::fs::read_dir(dev_dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with("media"))
                .collect()
        })
        .unwrap_or_default();
    media.sort();
    format!("{}|{}", v4l.join(","), media.join(","))
}

/// The fingerprint of the running system.
pub fn fingerprint() -> String {
    fingerprint_in(Path::new(SYS_VIDEO4LINUX), Path::new(DEV_DIR))
}

/// Whether a cold start must run full discovery.
///
/// Always when the last discovery found a camera: that re-probe is what
/// renegotiates a replugged camera's input format. Otherwise only when the
/// fingerprint changed, a hotplug asked for it, there is no previous run, or
/// the last run is older than [`DISCOVERY_CEILING`].
pub fn should_rediscover(
    had_camera: bool,
    previous: Option<&str>,
    current: &str,
    hotplug: bool,
    since_last: Option<Duration>,
) -> bool {
    had_camera
        || hotplug
        || previous != Some(current)
        || since_last.is_none_or(|age| age >= DISCOVERY_CEILING)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_tracks_devices_and_their_links() {
        let root = tempfile::tempdir().unwrap();
        let sys = root.path().join("video4linux");
        let dev = root.path().join("dev");
        std::fs::create_dir_all(&sys).unwrap();
        std::fs::create_dir_all(&dev).unwrap();
        let empty = fingerprint_in(&sys, &dev);
        assert_eq!(empty, "|");
        // Missing roots read as empty rather than failing.
        assert_eq!(
            fingerprint_in(&root.path().join("nope"), &root.path().join("nope")),
            "|"
        );

        let video0 = sys.join("video0");
        std::fs::create_dir_all(&video0).unwrap();
        std::os::unix::fs::symlink("../../usb1/1-1", video0.join("device")).unwrap();
        std::fs::write(dev.join("media0"), b"").unwrap();
        std::fs::write(dev.join("null"), b"").unwrap();
        let one = fingerprint_in(&sys, &dev);
        assert_eq!(one, "video0=../../usb1/1-1|media0");

        // The same node name re-enumerated on another port is a change.
        std::fs::remove_file(video0.join("device")).unwrap();
        std::os::unix::fs::symlink("../../usb2/2-1", video0.join("device")).unwrap();
        assert_ne!(fingerprint_in(&sys, &dev), one);
    }

    #[test]
    fn discovery_is_gated_only_on_a_camera_less_node() {
        let fresh = Some(Duration::from_secs(5));
        // A present camera re-probes on every start.
        assert!(should_rediscover(true, Some("a"), "a", false, fresh));
        // Camera-less, unchanged, recent, no hotplug: skip.
        assert!(!should_rediscover(false, Some("a"), "a", false, fresh));
        // Any trigger runs it.
        assert!(should_rediscover(false, Some("a"), "b", false, fresh));
        assert!(should_rediscover(false, Some("a"), "a", true, fresh));
        assert!(should_rediscover(false, None, "a", false, fresh));
        assert!(should_rediscover(false, Some("a"), "a", false, None));
        assert!(should_rediscover(
            false,
            Some("a"),
            "a",
            false,
            Some(DISCOVERY_CEILING)
        ));
    }
}
