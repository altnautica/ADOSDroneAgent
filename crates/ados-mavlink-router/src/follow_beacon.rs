//! The operator's shared phone position (`plugin.follow.beacon`).
//!
//! A ground station that the operator lets share its position sends MAVLink
//! `FOLLOW_TARGET` (#144) toward the vehicle. Every client entry point (the
//! MAVLink socket, the WebSocket, TCP and UDP proxies, the aux uplink) hands
//! its bytes to [`observe`] with the writer as the router classified it
//! ([`link`]), before they are routed anywhere; each `FOLLOW_TARGET` frame is
//! judged by the [`gate`]:
//!
//! * only from an authorised ground link, a local service or a relay; never an
//!   unauthenticated network peer, a plugin, the plugin host's link (which
//!   declares that it relays plugin traffic) or an autonomous injector;
//! * one link at a time, until it has been silent for [`LOCK_SILENCE_MS`];
//! * at most one fix per [`MIN_INTERVAL_MS`] (10 Hz) on that link;
//! * not older than [`MAX_FIX_AGE_MS`] against the link's least-delayed fix,
//!   and newer than its last;
//! * with a known horizontal accuracy no worse than [`MAX_HORIZ_ACC_M`].
//!
//! An accepted fix becomes a [`FollowBeacon`] stamped with the router's ADOS
//! time, origin and link id, and goes to the plugin host's event export
//! ([`export`]), which puts it on the bus as `plugin.follow.beacon`.
//!
//! The message itself goes nowhere else: [`strip`] takes `FOLLOW_TARGET` out
//! of every client write before route-aware forwarding hands it to the other
//! clients (the MAVLink socket, the cloud relay), and before the flight
//! controller write and the aux uplink, unless `mavlink.follow_target_to_fc`
//! opts the flight controller in (for its own Follow mode). A frame split
//! across two writes is not recognised, so it is neither judged nor stripped.
//!
//! The counts per reason are in the router status (`follow_beacon`).
//!
//! [`MIN_INTERVAL_MS`]: ados_protocol::follow_beacon::MIN_INTERVAL_MS
//! [`MAX_FIX_AGE_MS`]: ados_protocol::follow_beacon::MAX_FIX_AGE_MS
//! [`MAX_HORIZ_ACC_M`]: ados_protocol::follow_beacon::MAX_HORIZ_ACC_M
//! [`LOCK_SILENCE_MS`]: ados_protocol::follow_beacon::LOCK_SILENCE_MS

pub mod export;
pub mod gate;
pub mod link;

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use ados_protocol::follow_beacon::{FollowBeacon, FOLLOW_TARGET_MSG_ID};
use ados_protocol::mavlink::MavMessage;
use tokio::sync::mpsc;
// Fix age and the rate limit are judged on steady wall time: the sender's
// clock is a wall clock, whatever ADOS time does.
use tokio::time::Instant;

use export::ExportStats;
use gate::{BeaconGate, Claimed};
pub use link::{BeaconLink, LinkId, Origin};

/// Beacons waiting for the export task. At 10 Hz this is over a second.
const QUEUE: usize = 16;

/// `FOLLOW_TARGET` frames taken out of client writes, process-wide.
static STRIPPED: AtomicU64 = AtomicU64::new(0);

/// The process-wide beacon path: the gate and the export queue.
pub struct FollowBeaconLink {
    gate: Mutex<BeaconGate>,
    tx: mpsc::Sender<FollowBeacon>,
    export: Arc<ExportStats>,
    wall0: Instant,
}

static LINK: OnceLock<FollowBeaconLink> = OnceLock::new();

/// The process-wide link, once [`start`] ran.
pub fn global() -> Option<&'static FollowBeaconLink> {
    LINK.get()
}

/// Start the link and its export task (drone profile), publishing to the
/// export socket at `path`.
pub fn start(path: PathBuf) {
    let (link, rx) = FollowBeaconLink::new();
    let stats = link.export.clone();
    if LINK.set(link).is_ok() {
        tokio::spawn(export::run(path, rx, stats));
    }
}

/// Judge every `FOLLOW_TARGET` frame in one client write from `link` (a no-op
/// until [`start`] ran). `ados_ns` is the router's ADOS time now.
pub fn observe(link: BeaconLink, data: &[u8], ados_ns: u64) {
    if let Some(beacon) = global() {
        beacon.observe(link, data, ados_ns);
    }
}

/// Each whole MAVLink frame at the head of `data`, until bytes that are not a
/// whole frame; those are left to the caller.
fn frames(data: &[u8]) -> impl Iterator<Item = (usize, &[u8])> {
    let mut i = 0;
    std::iter::from_fn(move || {
        let rest = &data[i..];
        let n = crate::pilot_link::frame_len(rest).filter(|&n| n >= 8 && n <= rest.len())?;
        let at = i;
        i += n;
        Some((at, &rest[..n]))
    })
}

fn is_follow_target(frame: &[u8]) -> bool {
    crate::pilot_link::frame_id(frame) == FOLLOW_TARGET_MSG_ID
}

/// `data` without its `FOLLOW_TARGET` frames; borrowed unchanged when it has
/// none. Bytes after the last whole frame are kept as they are.
pub fn strip(data: &[u8]) -> Cow<'_, [u8]> {
    if !frames(data).any(|(_, f)| is_follow_target(f)) {
        return Cow::Borrowed(data);
    }
    let mut out = Vec::with_capacity(data.len());
    let mut end = 0;
    for (at, frame) in frames(data) {
        end = at + frame.len();
        if is_follow_target(frame) {
            STRIPPED.fetch_add(1, Ordering::Relaxed);
        } else {
            out.extend_from_slice(frame);
        }
    }
    out.extend_from_slice(&data[end..]);
    Cow::Owned(out)
}

impl FollowBeaconLink {
    /// A link and the receiving end of its export queue.
    pub fn new() -> (Self, mpsc::Receiver<FollowBeacon>) {
        let (tx, rx) = mpsc::channel(QUEUE);
        let link = Self {
            gate: Mutex::new(BeaconGate::default()),
            tx,
            export: Arc::new(ExportStats::default()),
            wall0: Instant::now(),
        };
        (link, rx)
    }

    /// Judge every `FOLLOW_TARGET` frame in one client write from `link`.
    pub fn observe(&self, link: BeaconLink, data: &[u8], ados_ns: u64) {
        let wall_ms = self.wall0.elapsed().as_millis() as u64;
        self.observe_at(link, data, wall_ms, ados_ns);
    }

    /// [`Self::observe`] at an explicit steady wall time, in ms.
    pub fn observe_at(&self, link: BeaconLink, data: &[u8], wall_ms: u64, ados_ns: u64) {
        for (_, frame) in frames(data).filter(|(_, f)| is_follow_target(f)) {
            let Ok((header, MavMessage::FOLLOW_TARGET(msg))) =
                ados_protocol::mavlink::parse_any(frame)
            else {
                continue;
            };
            let claimed = Claimed {
                sysid: header.system_id,
                compid: header.component_id,
            };
            let verdict = self
                .gate
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .admit(link, claimed, &msg, wall_ms, ados_ns);
            match verdict {
                Ok(beacon) => {
                    if self.tx.try_send(beacon).is_err() {
                        self.export.dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Err(reason) => {
                    tracing::debug!(?reason, origin = ?link.origin, "follow_beacon_fix_dropped");
                }
            }
        }
    }

    /// The router-status block: the gate's counts, the export's, and the
    /// frames kept off the other endpoints.
    pub fn status(&self) -> serde_json::Value {
        let counters = self.gate.lock().unwrap_or_else(|p| p.into_inner()).counters;
        let mut v = serde_json::to_value(counters).unwrap_or_default();
        if let (Some(obj), Ok(serde_json::Value::Object(ex))) = (
            v.as_object_mut(),
            serde_json::to_value(self.export.snapshot()),
        ) {
            obj.extend(ex);
            obj.insert("stripped".into(), STRIPPED.load(Ordering::Relaxed).into());
        }
        v
    }
}

#[cfg(test)]
mod tests;
