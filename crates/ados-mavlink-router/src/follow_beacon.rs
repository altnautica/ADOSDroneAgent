//! The operator's shared phone position (`plugin.follow.beacon`).
//!
//! A ground station that the operator lets share its position sends MAVLink
//! `FOLLOW_TARGET` (#144) toward the vehicle. Every client write passes
//! [`FollowBeaconLink::observe`] on its way to the flight controller (the bytes
//! themselves still go there unchanged); each `FOLLOW_TARGET` frame in it is
//! judged by the [`gate`]:
//!
//! * only from an authorised ground link: a peer that cleared the proxy
//!   posture check, an on-box IPC writer, or the relay (the rules every client
//!   write follows, [`ClientOrigin`]); never an unauthenticated network peer,
//!   and never an autonomous injector (a plugin or the pilot);
//! * at most one fix per [`MIN_INTERVAL_MS`] (10 Hz);
//! * not older than [`MAX_FIX_AGE_MS`] against the sender's freshest fix, and
//!   newer than its last;
//! * with a known horizontal accuracy no worse than [`MAX_HORIZ_ACC_M`].
//!
//! An accepted fix becomes a [`FollowBeacon`] stamped with the router's ADOS
//! time and goes to the plugin host's event export ([`export`]), which puts it
//! on the bus as `plugin.follow.beacon`. The counts per reason are in the
//! router status (`follow_beacon`).
//!
//! [`MIN_INTERVAL_MS`]: ados_protocol::follow_beacon::MIN_INTERVAL_MS
//! [`MAX_FIX_AGE_MS`]: ados_protocol::follow_beacon::MAX_FIX_AGE_MS
//! [`MAX_HORIZ_ACC_M`]: ados_protocol::follow_beacon::MAX_HORIZ_ACC_M

pub mod export;
pub mod gate;

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock};

use ados_protocol::follow_beacon::{FollowBeacon, FOLLOW_TARGET_MSG_ID};
use ados_protocol::mavlink::MavMessage;
use tokio::sync::mpsc;
// Fix age and the rate limit are judged on steady wall time: the sender's
// clock is a wall clock, whatever ADOS time does.
use tokio::time::Instant;

use crate::connection::ClientOrigin;
use export::ExportStats;
use gate::{BeaconGate, Writer};

/// Beacons waiting for the export task. At 10 Hz this is over a second.
const QUEUE: usize = 16;

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

    /// Judge every `FOLLOW_TARGET` frame in one client write. `injector` is
    /// whether the writer is an autonomous injector; `ados_ns` is the router's
    /// ADOS time now.
    pub fn observe(&self, data: &[u8], origin: ClientOrigin, injector: bool, ados_ns: u64) {
        let wall_ms = self.wall0.elapsed().as_millis() as u64;
        self.observe_at(data, origin, injector, wall_ms, ados_ns);
    }

    /// [`Self::observe`] at an explicit steady wall time, in ms.
    pub fn observe_at(
        &self,
        data: &[u8],
        origin: ClientOrigin,
        injector: bool,
        wall_ms: u64,
        ados_ns: u64,
    ) {
        let mut i = 0;
        while i < data.len() {
            let rest = &data[i..];
            let Some(n) = crate::pilot_link::frame_len(rest).filter(|&n| n >= 8 && n <= rest.len())
            else {
                return;
            };
            let frame = &rest[..n];
            i += n;
            if crate::pilot_link::frame_id(frame) != FOLLOW_TARGET_MSG_ID {
                continue;
            }
            let Ok((header, MavMessage::FOLLOW_TARGET(msg))) =
                ados_protocol::mavlink::parse_any(frame)
            else {
                continue;
            };
            let writer = Writer {
                origin,
                injector,
                sysid: header.system_id,
                compid: header.component_id,
            };
            let verdict = self
                .gate
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .admit(writer, &msg, wall_ms, ados_ns);
            match verdict {
                Ok(beacon) => {
                    if self.tx.try_send(beacon).is_err() {
                        self.export.dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Err(reason) => {
                    tracing::debug!(?reason, sysid = writer.sysid, "follow_beacon_fix_dropped");
                }
            }
        }
    }

    /// The router-status block: the gate's counts and the export's.
    pub fn status(&self) -> serde_json::Value {
        let counters = self.gate.lock().unwrap_or_else(|p| p.into_inner()).counters;
        let mut v = serde_json::to_value(counters).unwrap_or_default();
        if let (Some(obj), Ok(serde_json::Value::Object(ex))) = (
            v.as_object_mut(),
            serde_json::to_value(self.export.snapshot()),
        ) {
            obj.extend(ex);
        }
        v
    }
}

#[cfg(test)]
mod tests;
