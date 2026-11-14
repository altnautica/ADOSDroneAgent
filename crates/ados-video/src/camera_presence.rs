//! Per-camera presence for the `camera-state.json` sidecar.
//!
//! Discovery answers only for the camera the pipeline opens itself. A camera
//! the vision engine captures (`capture: true`) is never probed, and a
//! secondary leg is not the primary discovery picks, so neither had a state of
//! its own: a captured primary read `ready` from a synthetic frame-bus entry
//! whatever its device did, and a lost belly camera read as nothing at all.
//! This module gives every local camera a state from what is really there: its
//! V4L2 node, or the USB fingerprint its roster entry pins, and, for a captured
//! camera, whether the vision engine's source for it is delivering.
//!
//! States: `ready`; `missing` when the device is gone, or is enumerated but
//! went silent under the vision engine (a wedged device, which a USB rebind
//! recovers); `error` when the engine reports any other source failure (an
//! open or format failure, or frames that arrive but do not decode, none of
//! which a USB rebind fixes).
//!
//! Two cameras of one model without serial numbers share a USB fingerprint.
//! Such a leg is located by its own source node when that node is on a device
//! with the fingerprint's ids. Otherwise the fingerprint names a node only when
//! exactly one enumerated device matches it, no other entry holds that device,
//! and no other unlocated entry carries the same fingerprint; when the match is
//! ambiguous the camera gets no `device_path`, so recovery never acts on a
//! camera that may be another entry's.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use ados_protocol::vision_status::{VisionStatus, VISION_STATUS_FILE};
use serde::Serialize;

use crate::camera_state::CameraState;
use crate::config::{primary_leg_index, CameraLeg};

/// How long the vision engine's status sidecar may go unwritten and still
/// speak for its sources. The engine rewrites it about once a second; an
/// older file belongs to an engine that is gone, whose last word is not the
/// camera's state.
pub const VISION_STATUS_FRESH: Duration = Duration::from_secs(10);

/// The id a node with no `cameras` list publishes its single camera under.
const SINGLE_CAMERA_ID: &str = "main";

/// Where presence is read from (injected so tests are hermetic).
#[derive(Debug, Clone)]
pub struct PresenceSources {
    /// `/sys/class/video4linux`.
    pub sys_v4l: PathBuf,
    /// The vision engine's status sidecar.
    pub vision_status: PathBuf,
}

impl PresenceSources {
    /// The running system's sources.
    pub fn system() -> Self {
        Self {
            sys_v4l: PathBuf::from(crate::sysfs_fingerprint::SYS_VIDEO4LINUX),
            vision_status: ados_protocol::paths::run_dir().join(VISION_STATUS_FILE),
        }
    }
}

/// One local camera's presence, as the sidecar carries it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CameraPresence {
    /// The roster id (`main` on a node with no `cameras` list).
    pub id: String,
    pub role: Option<String>,
    /// The transport primary (the camera served at `main`).
    pub primary: bool,
    /// The vision engine captures it (`capture: true`).
    pub captured: bool,
    /// The V4L2 node the camera is on; the configured locator while it is gone.
    pub device_path: Option<String>,
    pub state: CameraState,
    /// Why the state is not `ready`.
    pub reason: Option<String>,
}

/// What discovery said about the primary the pipeline opens itself.
#[derive(Debug, Clone)]
pub struct DiscoveredPrimary {
    pub state: CameraState,
    pub device_path: Option<String>,
}

/// The presence of every local camera in `cameras` (the configured roster).
///
/// A primary the pipeline opens itself mirrors discovery, which already judges
/// it (including a node that enumerates but no longer opens). A captured
/// camera and a local secondary leg are judged here. A leg with no device to
/// watch (a `csi` / `usb` hint with no USB fingerprint) and a network leg are
/// left out; so is a disabled secondary, which runs nothing.
pub fn assess(
    cameras: &[CameraLeg],
    discovered: &DiscoveredPrimary,
    sources: &PresenceSources,
) -> Vec<CameraPresence> {
    let Some(primary) = primary_leg_index(cameras) else {
        return vec![mirror(SINGLE_CAMERA_ID, None, discovered)];
    };
    let vision = read_vision_status(&sources.vision_status, SystemTime::now());
    let sys_v4l = &sources.sys_v4l;
    let plans: Vec<Plan> = cameras
        .iter()
        .enumerate()
        .map(|(i, leg)| {
            let is_primary = i == primary;
            if is_primary && !leg.capture {
                Plan::Mirror
            } else if (!leg.enabled && !is_primary) || leg.network_source().is_some() {
                Plan::Skip
            } else {
                locate(leg, sys_v4l).map_or(Plan::Skip, Plan::Judge)
            }
        })
        .collect();
    // The USB devices an entry already holds: a leg's own source node, and the
    // node discovery opened for a mirrored primary.
    let mut claimed: HashSet<PathBuf> = plans
        .iter()
        .filter_map(|p| match p {
            Plan::Judge(l) => l.pinned.as_ref()?.usb.clone(),
            _ => None,
        })
        .collect();
    if plans.iter().any(|p| matches!(p, Plan::Mirror)) {
        let mirrored = discovered
            .device_path
            .as_deref()
            .filter(|p| Path::new(p).is_absolute())
            .and_then(|p| node_usb_dir(sys_v4l, p));
        claimed.extend(mirrored);
    }
    let usb = usb_cameras(sys_v4l);
    let mut out = Vec::new();
    for (i, (leg, plan)) in cameras.iter().zip(&plans).enumerate() {
        match plan {
            Plan::Skip => {}
            Plan::Mirror => out.push(mirror(&leg.id, leg.role.clone(), discovered)),
            Plan::Judge(loc) => {
                // Unlocated entries carrying this fingerprint, this one included.
                let sharing = plans
                    .iter()
                    .filter(|p| match p {
                        Plan::Judge(o) => o.pinned.is_none() && o.want() == loc.want(),
                        _ => false,
                    })
                    .count();
                let found = loc.resolve(&usb, &claimed, sharing);
                out.push(judge(leg, i == primary, loc, found, vision.as_ref()));
            }
        }
    }
    out
}

fn mirror(id: &str, role: Option<String>, discovered: &DiscoveredPrimary) -> CameraPresence {
    CameraPresence {
        id: id.to_string(),
        role,
        primary: true,
        captured: false,
        device_path: discovered.device_path.clone(),
        state: discovered.state,
        reason: None,
    }
}

/// What becomes of one roster entry.
enum Plan<'a> {
    /// The primary the pipeline discovers itself.
    Mirror,
    /// A leg judged here.
    Judge(Locator<'a>),
    /// Nothing to watch.
    Skip,
}

/// What locates a leg the pipeline does not discover.
struct Locator<'a> {
    /// The configured source (a device path, or a `csi` / `usb` hint).
    source: &'a str,
    by_path: bool,
    /// The USB fingerprint as configured, and split into lowercase parts.
    usb: Option<(&'a str, Vec<String>)>,
    /// The leg's own source node when it is the camera: it exists and, under
    /// a USB fingerprint, sits on a device with the fingerprint's ids.
    pinned: Option<Node>,
}

/// A V4L2 node and the USB device it hangs off, if any.
struct Node {
    path: String,
    usb: Option<PathBuf>,
}

/// Where a leg's camera was found.
enum Found {
    Node(String),
    /// Enumerated devices match, but none is provably this camera.
    Ambiguous,
    Gone,
}

fn locate<'a>(leg: &'a CameraLeg, sys_v4l: &Path) -> Option<Locator<'a>> {
    let usb = leg
        .camera_match
        .as_ref()
        .and_then(|m| m.usb.as_deref())
        .filter(|u| !u.trim().is_empty())
        .map(|fp| {
            let parts: Vec<String> = fp.split(':').map(|s| s.trim().to_lowercase()).collect();
            (fp, parts)
        });
    let source = leg.source.trim();
    let by_path = Path::new(source).is_absolute();
    if usb.is_none() && !by_path {
        return None;
    }
    let pinned = by_path
        .then(|| std::fs::canonicalize(source).ok())
        .flatten()
        .and_then(|p| {
            let path = p.to_string_lossy().into_owned();
            let dir = node_usb_dir(sys_v4l, &path);
            match &usb {
                None => Some(Node { path, usb: dir }),
                Some((_, want)) => dir
                    .filter(|d| ids_match(want, d))
                    .map(|d| Node { path, usb: Some(d) }),
            }
        });
    Some(Locator {
        source,
        by_path,
        usb,
        pinned,
    })
}

impl Locator<'_> {
    fn want(&self) -> Option<&[String]> {
        self.usb.as_ref().map(|(_, w)| w.as_slice())
    }

    /// The leg's own node; else the one free device its fingerprint names,
    /// when no other unlocated entry (`sharing` counts this one) could be it.
    fn resolve(&self, usb: &[UsbCamera], claimed: &HashSet<PathBuf>, sharing: usize) -> Found {
        if let Some(n) = &self.pinned {
            return Found::Node(n.path.clone());
        }
        let Some(want) = self.want() else {
            return Found::Gone;
        };
        let mut free = usb
            .iter()
            .filter(|c| !claimed.contains(&c.dir) && ids_match(want, &c.dir));
        match (free.next(), free.next()) {
            (None, _) => Found::Gone,
            (Some(one), None) if sharing <= 1 => Found::Node(one.node.clone()),
            _ => Found::Ambiguous,
        }
    }
}

/// Judge one leg the pipeline does not discover.
fn judge(
    leg: &CameraLeg,
    primary: bool,
    loc: &Locator,
    found: Found,
    vision: Option<&VisionStatus>,
) -> CameraPresence {
    let failure = vision
        .filter(|_| leg.capture)
        .and_then(|v| v.source_errors.iter().find(|e| e.camera == leg.id));
    let present = !matches!(found, Found::Gone);
    let (state, reason) = match (present, failure) {
        (false, _) => (
            CameraState::Missing,
            Some(format!(
                "camera device {} is gone",
                loc.usb.as_ref().map_or(loc.source, |(fp, _)| *fp)
            )),
        ),
        (true, Some(e)) if e.is_silence() => (CameraState::Missing, Some(e.error.clone())),
        (true, Some(e)) => (CameraState::Error, Some(e.error.clone())),
        (true, None) => (CameraState::Ready, None),
    };
    // A gone camera keeps its configured locator named, so recovery knows what
    // it is looking for, unless that path is now some other device.
    let device_path = match found {
        Found::Node(n) => Some(n),
        Found::Ambiguous => None,
        Found::Gone => {
            let gone = loc.by_path && !Path::new(loc.source).exists();
            gone.then(|| loc.source.to_string())
        }
    };
    CameraPresence {
        id: leg.id.clone(),
        role: leg.role.clone(),
        primary,
        captured: leg.capture,
        device_path,
        state,
        reason,
    }
}

/// An enumerated USB camera: its device directory and its lowest-numbered
/// `/dev/videoN` node (a UVC camera also exposes a metadata node).
struct UsbCamera {
    dir: PathBuf,
    node: String,
}

/// Every USB camera in the sysfs class directory, once each.
fn usb_cameras(sys_v4l: &Path) -> Vec<UsbCamera> {
    let Ok(entries) = std::fs::read_dir(sys_v4l) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect();
    names.sort_by_key(|n| {
        (
            n.trim_start_matches(|c: char| !c.is_ascii_digit())
                .parse::<u32>()
                .unwrap_or(u32::MAX),
            n.clone(),
        )
    });
    let mut out: Vec<UsbCamera> = Vec::new();
    for name in names {
        let Some(dir) = class_usb_dir(sys_v4l, &name) else {
            continue;
        };
        if out.iter().all(|c| c.dir != dir) {
            out.push(UsbCamera {
                dir,
                node: format!("/dev/{name}"),
            });
        }
    }
    out
}

/// The USB device the class entry `name` (`videoN`) hangs off.
fn class_usb_dir(sys_v4l: &Path, name: &str) -> Option<PathBuf> {
    let device = std::fs::canonicalize(sys_v4l.join(name).join("device")).ok()?;
    usb_device_dir(&device)
}

/// The USB device the V4L2 node at `node` hangs off.
fn node_usb_dir(sys_v4l: &Path, node: &str) -> Option<PathBuf> {
    class_usb_dir(sys_v4l, Path::new(node).file_name()?.to_str()?)
}

/// Whether the USB device at `dir` carries the fingerprint `want` (`vid:pid`
/// or `vid:pid:serial`, lowercase hex).
fn ids_match(want: &[String], dir: &Path) -> bool {
    let read = |attr: &str| {
        std::fs::read_to_string(dir.join(attr))
            .map(|s| s.trim().to_lowercase())
            .unwrap_or_default()
    };
    let have = [read("idVendor"), read("idProduct"), read("serial")];
    (2..=3).contains(&want.len()) && want.iter().zip(have.iter()).all(|(w, h)| w == h)
}

/// The first directory at or above `start` holding a USB device's id files.
fn usb_device_dir(start: &Path) -> Option<PathBuf> {
    const MAX_HOPS: usize = 8;
    let mut dir = start.to_path_buf();
    for _ in 0..=MAX_HOPS {
        if dir.join("idVendor").is_file() && dir.join("idProduct").is_file() {
            return Some(dir);
        }
        dir = dir.parent()?.to_path_buf();
    }
    None
}

/// The vision engine's status, when its sidecar was written within
/// [`VISION_STATUS_FRESH`] of `now`.
fn read_vision_status(path: &Path, now: SystemTime) -> Option<VisionStatus> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let age = now.duration_since(modified).unwrap_or_default();
    if age > VISION_STATUS_FRESH {
        return None;
    }
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

#[cfg(test)]
#[path = "camera_presence_tests.rs"]
mod tests;
