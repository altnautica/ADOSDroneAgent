//! The mission mirror: the router's copy of the flight controller's main
//! mission, and the mission state computed from the frames the router sees.
//!
//! **Passive capture.** An FC-emitted `MISSION_COUNT` followed by the complete
//! `MISSION_ITEM_INT` run (a ground station downloading, or this mirror's own
//! download) replaces the items. A client upload through the router is captured
//! from the client's items and adopted when the FC accepts it.
//!
//! **Active download**, as this router's own component, through its own send
//! path: `MISSION_REQUEST_LIST`, `MISSION_REQUEST_INT` per seq, `MISSION_ACK`.
//! Triggered at the first FC heartbeat, after an upload the passive capture
//! could not complete, and when `MISSION_CURRENT.total` disagrees with the
//! mirror. It starts only when no other component has had a transfer in flight
//! for [`QUIET_BEFORE_DOWNLOAD`], aborts the moment one appears, waits
//! [`ITEM_TIMEOUT`] per reply with [`RETRIES`] retries, and backs off 2, 4, 8,
//! 16, then 30 s after a failure.
//!
//! The core ([`MirrorCore`]) is synchronous and clock-injected; the task in
//! [`run`] feeds it frames and ticks and writes the result into the
//! [`VehicleState`] the state socket publishes.

pub mod status;
pub mod wire;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ados_protocol::flight_modes::{ArduPilotFirmware, MAV_AUTOPILOT_PX4};
use ados_protocol::mavlink::ardupilotmega::{
    MavMessage, MavMissionResult, MISSION_ACK_DATA, MISSION_REQUEST_INT_DATA,
    MISSION_REQUEST_LIST_DATA,
};
use ados_protocol::shutdown::Shutdown;
use bytes::Bytes;
use serde_json::{json, Value};
use tokio::sync::{broadcast, Mutex};

use crate::connection::FcConnection;
use crate::state::VehicleState;
use status::{MissionStatus, MissionTracker, ModeTable, Vehicle};
use wire::{ItemInt, MissionCurrent, RawMsg};

/// Quiet period required before a download starts.
pub const QUIET_BEFORE_DOWNLOAD: Duration = Duration::from_secs(3);
/// How long one request waits for its reply.
pub const ITEM_TIMEOUT: Duration = Duration::from_secs(1);
/// Retries per request before the download fails.
pub const RETRIES: u8 = 3;
/// Backoff after successive failures.
const BACKOFF_S: [u64; 5] = [2, 4, 8, 16, 30];
/// A capture with no progress for this long is dropped.
const CAPTURE_STALE: Duration = Duration::from_secs(10);
/// The mirror file under the runtime directory, rewritten on every change of
/// the mirror. The mission state is not in it: that lives in the state
/// snapshot, which is current to 100 ms.
pub const MIRROR_FILE: &str = "mission-mirror.json";
/// Largest mission the mirror holds.
pub const MAX_ITEMS: usize = 1024;
/// The navigation commands the state snapshot carries, at most
/// [`STATE_ITEMS_CAP`] of them.
pub const NAV_COMMANDS: [u16; 7] = [16, 17, 18, 19, 21, 22, 82];
pub const STATE_ITEMS_CAP: usize = 256;

/// The mirrored mission.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Mirror {
    /// Bumped on every replacement.
    pub rev: u32,
    /// Whether a mirror exists (an empty mission is a mirror too).
    pub present: bool,
    pub items: Vec<ItemInt>,
}

/// An in-progress capture of a mission run.
#[derive(Debug, Clone)]
struct Capture {
    count: u16,
    items: BTreeMap<u16, ItemInt>,
    last: Instant,
}

impl Capture {
    fn new(count: u16, now: Instant) -> Self {
        Self {
            count: count.min(MAX_ITEMS as u16),
            items: BTreeMap::new(),
            last: now,
        }
    }

    fn put(&mut self, item: ItemInt, now: Instant) {
        if item.seq < self.count {
            self.items.insert(item.seq, item);
            self.last = now;
        }
    }

    fn complete(&self) -> bool {
        self.items.len() == usize::from(self.count)
    }
}

/// Where an active download stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Download {
    Idle,
    /// Waiting for MISSION_COUNT.
    List {
        sent: Instant,
        tries: u8,
    },
    /// Waiting for item `seq`.
    Item {
        seq: u16,
        sent: Instant,
        tries: u8,
    },
}

/// The synchronous mirror and mission-state core.
#[derive(Debug)]
pub struct MirrorCore {
    /// This router's own `(system, component)`.
    ours: (u8, u8),
    /// The vehicle `(system, component)`, latched from its HEARTBEAT.
    vehicle: Option<(u8, u8)>,
    autopilot: u8,
    mav_type: u8,
    armed: bool,
    custom_mode: u32,
    firmware: Option<ArduPilotFirmware>,
    pub mirror: Mirror,
    fc_capture: Option<Capture>,
    upload_capture: Option<Capture>,
    download: Download,
    want_download: bool,
    not_before: Option<Instant>,
    failures: usize,
    foreign_at: Option<Instant>,
    pub tracker: MissionTracker,
    last_total: Option<u16>,
    last_seq_probe: Option<u16>,
    /// Download outcomes, for the tests and the log.
    pub downloads_ok: u32,
    pub downloads_failed: u32,
}

impl MirrorCore {
    pub fn new(ours: (u8, u8)) -> Self {
        Self {
            ours,
            vehicle: None,
            autopilot: 0,
            mav_type: 0,
            armed: false,
            custom_mode: 0,
            firmware: None,
            mirror: Mirror::default(),
            fc_capture: None,
            upload_capture: None,
            download: Download::Idle,
            want_download: false,
            not_before: None,
            failures: 0,
            foreign_at: None,
            tracker: MissionTracker::default(),
            last_total: None,
            last_seq_probe: None,
            downloads_ok: 0,
            downloads_failed: 0,
        }
    }

    /// The firmware the FC banner named, from the state aggregator.
    pub fn set_firmware(&mut self, fw: Option<ArduPilotFirmware>) {
        self.firmware = fw;
    }

    pub fn table(&self) -> ModeTable {
        if u32::from(self.autopilot) == MAV_AUTOPILOT_PX4 as u32 {
            return ModeTable::Px4;
        }
        // A multirotor type with no banner yet is taken as Copter here: this
        // only labels the mission state, it never encodes a mode command.
        match self
            .firmware
            .or(ArduPilotFirmware::from_mav_type(i64::from(self.mav_type)))
        {
            Some(ArduPilotFirmware::Copter) => ModeTable::Copter,
            Some(ArduPilotFirmware::Plane) => ModeTable::Plane,
            _ => ModeTable::Other,
        }
    }

    fn is_vehicle(&self, m: &RawMsg<'_>) -> bool {
        self.vehicle == Some((m.sys, m.comp))
    }

    fn mark_foreign(&mut self, now: Instant) {
        self.foreign_at = Some(now);
        if self.download != Download::Idle {
            // Someone else started a transfer: get out of its way.
            self.download = Download::Idle;
            self.want_download = true;
            self.not_before = None;
        }
    }

    fn adopt(&mut self, capture: Capture) {
        self.mirror.rev = self.mirror.rev.wrapping_add(1);
        self.mirror.present = true;
        self.mirror.items = capture.items.into_values().collect();
        // A complete run is as good as our own download.
        if self.download == Download::Idle {
            self.want_download = false;
        }
    }

    /// One frame off the FC link.
    pub fn on_fc_frame(&mut self, m: &RawMsg<'_>, now: Instant) -> Vec<MavMessage> {
        use wire::*;
        if m.msg_id == MSG_HEARTBEAT {
            let autopilot = m.u8_at(5);
            let mav_type = m.u8_at(4);
            // MAV_AUTOPILOT_INVALID (8) and MAV_TYPE_GCS (6) are not vehicles.
            if autopilot != 8 && mav_type != 6 {
                let first = self.vehicle.is_none();
                self.vehicle = Some((m.sys, m.comp));
                self.autopilot = autopilot;
                self.mav_type = mav_type;
                let mode = m.u32_at(0);
                // Entering a mission mode with nothing mirrored: the plan
                // reached the FC some way the router could not see (another
                // link, or firmware that reports no `total`).
                if mode != self.custom_mode
                    && self.table().in_auto(mode)
                    && self.mirror.items.len() <= usize::from(self.table().home_slot())
                {
                    self.want_download = true;
                }
                self.custom_mode = mode;
                self.armed = m.u8_at(6) & 0x80 != 0;
                if first {
                    self.want_download = true;
                }
            }
            return Vec::new();
        }
        if !self.is_vehicle(m) {
            return Vec::new();
        }
        match m.msg_id {
            MSG_MISSION_CURRENT => {
                let mc = MissionCurrent::decode(m);
                self.tracker.on_mission_current(mc, now);
                self.check_total(mc);
                self.check_seq(mc.seq);
            }
            MSG_MISSION_ITEM_REACHED => self.tracker.on_item_reached(m.u16_at(0)),
            MSG_COMMAND_ACK if m.u16_at(0) == 193 => {
                self.tracker.on_pause_ack(m.u8_at(2) == 0, now);
            }
            MSG_MISSION_COUNT if mission_type(m) == 0 => {
                if target(m).1 != self.ours.1 {
                    self.mark_foreign(now);
                }
                self.fc_capture = Some(Capture::new(m.u16_at(0), now));
                if target(m).1 == self.ours.1 {
                    if let Download::List { .. } = self.download {
                        return self.after_count(now);
                    }
                }
            }
            MSG_MISSION_ITEM_INT if mission_type(m) == 0 => {
                let item = ItemInt::decode(m);
                let ours = target(m).1 == self.ours.1;
                if let Some(cap) = self.fc_capture.as_mut() {
                    cap.put(item, now);
                    if cap.complete() {
                        let cap = self.fc_capture.take().expect("present");
                        self.adopt(cap);
                        if ours && self.download != Download::Idle {
                            return self.finish_download();
                        }
                        return Vec::new();
                    }
                }
                if ours {
                    if let Download::Item { seq, .. } = self.download {
                        if item.seq == seq {
                            return self.request_item(seq + 1, now, 0);
                        }
                    }
                }
            }
            MSG_MISSION_REQUEST_INT | MSG_MISSION_REQUEST if target(m).1 != self.ours.1 => {
                self.mark_foreign(now);
            }
            MSG_MISSION_ACK if mission_type(m) == 0 && target(m).1 != self.ours.1 => {
                let accepted = m.u8_at(2) == 0;
                if self.tracker.on_fc_mission_ack(accepted, now) {
                    match self.upload_capture.take().filter(Capture::complete) {
                        Some(cap) => self.adopt(cap),
                        None => self.want_download = true,
                    }
                }
            }
            _ => {}
        }
        Vec::new()
    }

    /// One frame a client sent toward the FC through the router.
    pub fn on_client_frame(&mut self, m: &RawMsg<'_>, now: Instant) {
        use wire::*;
        match m.msg_id {
            MSG_COMMAND_LONG if m.u16_at(28) == 193 => {
                let p1 = f32::from_bits(m.u32_at(0));
                self.tracker.on_client_pause_command(p1, now);
            }
            MSG_MISSION_COUNT if mission_type(m) == 0 => {
                self.mark_foreign(now);
                self.tracker.on_client_upload_start(now);
                self.upload_capture = Some(Capture::new(m.u16_at(0), now));
            }
            MSG_MISSION_ITEM_INT if mission_type(m) == 0 => {
                self.mark_foreign(now);
                if let Some(cap) = self.upload_capture.as_mut() {
                    cap.put(ItemInt::decode(m), now);
                }
            }
            MSG_MISSION_REQUEST_LIST
            | MSG_MISSION_REQUEST_INT
            | MSG_MISSION_REQUEST
            | MSG_MISSION_ITEM
            | MSG_MISSION_ACK => self.mark_foreign(now),
            _ => {}
        }
    }

    fn check_total(&mut self, mc: MissionCurrent) {
        if mc.state == 0 || mc.total == u16::MAX {
            return;
        }
        let expect = usize::from(mc.total) + usize::from(self.table().home_slot());
        let have = self.mirror.items.len();
        let mismatch = !self.mirror.present || (have != expect && !(mc.total == 0 && have <= 1));
        if mismatch && self.last_total != Some(mc.total) {
            self.want_download = true;
        }
        self.last_total = Some(mc.total);
    }

    /// A current seq past the mirror's last item means the FC holds a plan
    /// the mirror has not seen (an upload the router could not see, on
    /// firmware whose MISSION_CURRENT carries no `total`). Asked once per seq.
    fn check_seq(&mut self, seq: u16) {
        if self.mirror.present
            && usize::from(seq) >= self.mirror.items.len()
            && self.last_seq_probe != Some(seq)
        {
            self.last_seq_probe = Some(seq);
            self.want_download = true;
        }
    }

    fn fc_target(&self) -> (u8, u8) {
        self.vehicle.unwrap_or((1, 1))
    }

    fn after_count(&mut self, now: Instant) -> Vec<MavMessage> {
        let count = self.fc_capture.as_ref().map_or(0, |c| c.count);
        if count == 0 {
            let cap = self.fc_capture.take().expect("present");
            self.adopt(cap);
            return self.finish_download();
        }
        self.request_item(0, now, 0)
    }

    fn request_item(&mut self, seq: u16, now: Instant, tries: u8) -> Vec<MavMessage> {
        let count = self.fc_capture.as_ref().map_or(0, |c| c.count);
        if seq >= count {
            // The run completed (adoption happens on the last item).
            return self.finish_download();
        }
        self.download = Download::Item {
            seq,
            sent: now,
            tries,
        };
        let (ts, tc) = self.fc_target();
        vec![MavMessage::MISSION_REQUEST_INT(MISSION_REQUEST_INT_DATA {
            seq,
            target_system: ts,
            target_component: tc,
        })]
    }

    fn finish_download(&mut self) -> Vec<MavMessage> {
        self.download = Download::Idle;
        self.failures = 0;
        self.downloads_ok += 1;
        let (ts, tc) = self.fc_target();
        vec![MavMessage::MISSION_ACK(MISSION_ACK_DATA {
            target_system: ts,
            target_component: tc,
            mavtype: MavMissionResult::MAV_MISSION_ACCEPTED,
        })]
    }

    fn fail_download(&mut self, now: Instant) {
        self.download = Download::Idle;
        self.downloads_failed += 1;
        let back = BACKOFF_S[self.failures.min(BACKOFF_S.len() - 1)];
        self.failures += 1;
        self.want_download = true;
        self.not_before = Some(now + Duration::from_secs(back));
    }

    /// Advance timers and start a wanted download; returns frames to send.
    pub fn tick(&mut self, now: Instant) -> Vec<MavMessage> {
        for cap in [&mut self.fc_capture, &mut self.upload_capture] {
            if cap
                .as_ref()
                .is_some_and(|c| now.saturating_duration_since(c.last) > CAPTURE_STALE)
            {
                *cap = None;
            }
        }
        let (ts, tc) = self.fc_target();
        match self.download {
            Download::List { sent, tries }
                if now.saturating_duration_since(sent) >= ITEM_TIMEOUT =>
            {
                if tries >= RETRIES {
                    self.fail_download(now);
                } else {
                    self.download = Download::List {
                        sent: now,
                        tries: tries + 1,
                    };
                    return vec![request_list(ts, tc)];
                }
            }
            Download::Item { seq, sent, tries }
                if now.saturating_duration_since(sent) >= ITEM_TIMEOUT =>
            {
                if tries >= RETRIES {
                    self.fail_download(now);
                } else {
                    return self.request_item(seq, now, tries + 1);
                }
            }
            _ => {}
        }
        let quiet = self
            .foreign_at
            .is_none_or(|at| now.saturating_duration_since(at) >= QUIET_BEFORE_DOWNLOAD);
        if self.download == Download::Idle
            && self.want_download
            && self.vehicle.is_some()
            && quiet
            && self.not_before.is_none_or(|t| now >= t)
        {
            self.want_download = false;
            self.not_before = None;
            self.download = Download::List {
                sent: now,
                tries: 0,
            };
            return vec![request_list(ts, tc)];
        }
        Vec::new()
    }

    /// The mission status now.
    pub fn status(&mut self, now: Instant) -> MissionStatus {
        let v = Vehicle {
            armed: self.armed,
            custom_mode: self.custom_mode,
            table: self.table(),
        };
        let items = self.mirror.present.then_some(self.mirror.items.len());
        self.tracker.compute(v, items, now)
    }

    /// The `mission` block of the state snapshot: the mission state plus the
    /// mirror (`rev`, the navigation items) and `uploads_seen`.
    pub fn state_json(&mut self, now: Instant) -> Value {
        let status = self.status(now);
        let mut v = status.to_json();
        let home_slot = self.table().home_slot();
        let items: Vec<Value> = self
            .mirror
            .items
            .iter()
            .filter(|i| !(home_slot && i.seq == 0))
            .filter(|i| NAV_COMMANDS.contains(&i.command))
            .take(STATE_ITEMS_CAP)
            .map(|i| {
                json!({
                    "seq": i.seq, "cmd": i.command, "frame": i.frame,
                    "lat_e7": i.x, "lon_e7": i.y, "alt_m": i.z,
                })
            })
            .collect();
        if let Value::Object(map) = &mut v {
            map.insert("rev".into(), json!(self.mirror.rev));
            map.insert("items".into(), Value::Array(items));
            map.insert("uploads_seen".into(), json!(self.tracker.uploads_seen()));
        }
        v
    }

    /// The mirror file body ([`MIRROR_FILE`]): the full mirror in
    /// MISSION_ITEM_INT field order. `GET /api/v1/mission` serves it with the
    /// live mission state added.
    pub fn mirror_json(&self) -> Value {
        json!({
            "rev": self.mirror.rev,
            "present": self.mirror.present,
            "count": self.mirror.items.len(),
            "home_slot": self.table().home_slot(),
            "items": self.mirror.items.iter().map(|i| json!({
                "param1": i.param1, "param2": i.param2, "param3": i.param3, "param4": i.param4,
                "x": i.x, "y": i.y, "z": i.z, "seq": i.seq, "command": i.command,
                "frame": i.frame, "current": i.current, "autocontinue": i.autocontinue,
                "mission_type": 0,
            })).collect::<Vec<_>>(),
        })
    }
}

fn request_list(ts: u8, tc: u8) -> MavMessage {
    MavMessage::MISSION_REQUEST_LIST(MISSION_REQUEST_LIST_DATA {
        target_system: ts,
        target_component: tc,
    })
}

/// Drive the core from the FC fan-out and the client tap, publish into the
/// state, and send the download frames through the router's own path.
pub async fn run(
    fc: Arc<FcConnection>,
    state: Arc<Mutex<VehicleState>>,
    mut client_rx: broadcast::Receiver<Bytes>,
    dir: String,
    cancel: Shutdown,
) {
    let mut core = MirrorCore::new(fc.our_ids());
    let path = std::path::Path::new(&dir).join(MIRROR_FILE);
    let mut written: Option<(u32, bool, bool)> = None;
    let mut fc_rx = fc.subscribe();
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let out = tokio::select! {
            frame = fc_rx.recv() => match frame {
                Ok(f) => {
                    let now = Instant::now();
                    let mut out = Vec::new();
                    for m in wire::frames(&f.bytes) {
                        out.extend(core.on_fc_frame(&m, now));
                    }
                    out
                }
                Err(broadcast::error::RecvError::Lagged(_)) => Vec::new(),
                Err(broadcast::error::RecvError::Closed) => return,
            },
            bytes = client_rx.recv() => match bytes {
                Ok(b) => {
                    let now = Instant::now();
                    for m in wire::frames(&b) {
                        core.on_client_frame(&m, now);
                    }
                    Vec::new()
                }
                Err(broadcast::error::RecvError::Lagged(_)) => Vec::new(),
                Err(broadcast::error::RecvError::Closed) => return,
            },
            _ = tick.tick() => {
                let now = Instant::now();
                let fw = { state.lock().await.firmware };
                core.set_firmware(fw);
                let out = core.tick(now);
                let mission = core.state_json(now);
                state.lock().await.mission = Some(mission);
                let key = (core.mirror.rev, core.mirror.present, core.table().home_slot());
                if written != Some(key) {
                    written = Some(key);
                    let body = core.mirror_json().to_string();
                    let path = path.clone();
                    tokio::task::spawn_blocking(move || {
                        if let Err(e) = crate::param_cache::write_atomic(&path, body.as_bytes()) {
                            tracing::warn!(error = %e, "mission_mirror_write_failed");
                        }
                    });
                }
                out
            }
            _ = cancel.wait() => return,
        };
        for msg in out {
            if !fc.send_own(&msg).await {
                tracing::debug!("mission_mirror_send_failed");
            }
        }
    }
}
