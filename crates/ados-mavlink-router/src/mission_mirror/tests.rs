use std::time::{Duration, Instant};

use ados_protocol::mavlink::ardupilotmega::MavMessage;

use super::status::MissionState;
use super::wire::{self, build::v2};
use super::*;

const FC: (u8, u8) = (1, 1);
const US: (u8, u8) = (1, 191);
const GCS: (u8, u8) = (255, 190);

/// A scripted link: feeds frames to a core and records what it sends.
struct Link {
    core: MirrorCore,
    t: Instant,
    sent: Vec<MavMessage>,
}

impl Link {
    fn new() -> Self {
        Self {
            core: MirrorCore::new(US),
            t: Instant::now(),
            sent: Vec::new(),
        }
    }

    fn adv(&mut self, ms: u64) {
        self.t += Duration::from_millis(ms);
        let out = self.core.tick(self.t);
        self.sent.extend(out);
    }

    fn fc(&mut self, msg_id: u32, payload: &[u8]) {
        let f = v2(FC.0, FC.1, msg_id, payload);
        for m in wire::frames(&f) {
            let out = self.core.on_fc_frame(&m, self.t);
            self.sent.extend(out);
        }
    }

    fn client(&mut self, msg_id: u32, payload: &[u8]) {
        let f = v2(GCS.0, GCS.1, msg_id, payload);
        for m in wire::frames(&f) {
            self.core.on_client_frame(&m, self.t);
        }
    }

    /// HEARTBEAT: `mav_type` 2 (quad) or 1 (fixed wing), autopilot 3 (ArduPilot).
    fn heartbeat(&mut self, mav_type: u8, custom_mode: u32, armed: bool) {
        let mut p = custom_mode.to_le_bytes().to_vec();
        p.extend_from_slice(&[mav_type, 3, if armed { 0x80 | 0x01 } else { 0x01 }, 4, 3]);
        self.fc(wire::MSG_HEARTBEAT, &p);
    }

    fn current(&mut self, seq: u16, total: u16, state: u8) {
        let mut p = seq.to_le_bytes().to_vec();
        p.extend_from_slice(&total.to_le_bytes());
        p.extend_from_slice(&[state, 0, 0, 0, 0, 0]);
        self.fc(wire::MSG_MISSION_CURRENT, &p);
    }

    fn state(&mut self) -> MissionState {
        self.core.status(self.t).state
    }

    fn take_sent(&mut self) -> Vec<MavMessage> {
        std::mem::take(&mut self.sent)
    }
}

fn count_payload(count: u16, target: (u8, u8)) -> Vec<u8> {
    let mut p = count.to_le_bytes().to_vec();
    p.extend_from_slice(&[target.0, target.1, 0]);
    p
}

fn item_payload(seq: u16, target: (u8, u8), cmd: u16) -> Vec<u8> {
    let mut p = Vec::new();
    for _ in 0..4 {
        p.extend_from_slice(&0f32.to_le_bytes());
    }
    p.extend_from_slice(&(470_000_000i32 + i32::from(seq)).to_le_bytes());
    p.extend_from_slice(&85_000_000i32.to_le_bytes());
    p.extend_from_slice(&30f32.to_le_bytes());
    p.extend_from_slice(&seq.to_le_bytes());
    p.extend_from_slice(&cmd.to_le_bytes());
    p.extend_from_slice(&[target.0, target.1, 3, 0, 1, 0]);
    p
}

/// A link with a mirrored 4-waypoint ArduPilot mission (home + 4 items).
fn copter_with_mission() -> Link {
    let mut l = Link::new();
    l.heartbeat(2, 0, false);
    l.fc(wire::MSG_MISSION_COUNT, &count_payload(5, GCS));
    for seq in 0..5 {
        l.fc(wire::MSG_MISSION_ITEM_INT, &item_payload(seq, GCS, 16));
    }
    assert_eq!(l.core.mirror.items.len(), 5);
    l
}

// -- mirror -----------------------------------------------------------------

#[test]
fn passive_capture_of_a_gcs_download() {
    let l = copter_with_mission();
    assert_eq!(l.core.mirror.rev, 1);
    assert!(l.core.mirror.present);
    assert_eq!(l.core.mirror.items[3].x, 470_000_003);
}

#[test]
fn state_json_carries_nav_items_without_the_home_slot() {
    let mut l = copter_with_mission();
    let t = l.t;
    let v = l.core.state_json(t);
    assert_eq!(v["rev"], 1);
    assert_eq!(v["items"].as_array().unwrap().len(), 4);
    assert_eq!(v["items"][0]["seq"], 1);
    assert_eq!(v["uploads_seen"], 0);
    let full = l.core.mirror_json();
    assert_eq!(full["items"].as_array().unwrap().len(), 5);
    assert_eq!(full["items"][0]["command"], 16);
}

#[test]
fn active_download_retries_a_dropped_item() {
    let mut l = Link::new();
    l.heartbeat(2, 0, false);
    l.adv(200);
    let sent = l.take_sent();
    assert!(matches!(sent.as_slice(), [MavMessage::MISSION_REQUEST_LIST(_)]));
    l.fc(wire::MSG_MISSION_COUNT, &count_payload(3, US));
    assert!(matches!(l.take_sent().as_slice(), [MavMessage::MISSION_REQUEST_INT(r)] if r.seq == 0));
    l.fc(wire::MSG_MISSION_ITEM_INT, &item_payload(0, US, 16));
    assert!(matches!(l.take_sent().as_slice(), [MavMessage::MISSION_REQUEST_INT(r)] if r.seq == 1));
    // Item 1 is dropped: after the timeout it is requested again.
    l.adv(1100);
    assert!(matches!(l.take_sent().as_slice(), [MavMessage::MISSION_REQUEST_INT(r)] if r.seq == 1));
    l.fc(wire::MSG_MISSION_ITEM_INT, &item_payload(1, US, 16));
    l.take_sent();
    l.fc(wire::MSG_MISSION_ITEM_INT, &item_payload(2, US, 16));
    assert!(matches!(l.take_sent().as_slice(), [MavMessage::MISSION_ACK(_)]));
    assert_eq!(l.core.mirror.items.len(), 3);
    assert_eq!(l.core.downloads_ok, 1);
}

#[test]
fn a_download_that_never_answers_fails_and_backs_off() {
    let mut l = Link::new();
    l.heartbeat(2, 0, false);
    l.adv(200);
    l.take_sent();
    for _ in 0..3 {
        l.adv(1000);
        assert!(matches!(l.take_sent().as_slice(), [MavMessage::MISSION_REQUEST_LIST(_)]));
    }
    l.adv(1000);
    assert_eq!(l.core.downloads_failed, 1);
    assert!(l.take_sent().is_empty());
    l.adv(1000);
    assert!(l.take_sent().is_empty(), "inside the 2 s backoff");
    l.adv(1100);
    assert!(matches!(l.take_sent().as_slice(), [MavMessage::MISSION_REQUEST_LIST(_)]));
}

#[test]
fn a_gcs_transfer_aborts_the_download_and_waits_for_quiet() {
    let mut l = Link::new();
    l.heartbeat(2, 0, false);
    l.adv(200);
    l.take_sent();
    l.fc(wire::MSG_MISSION_COUNT, &count_payload(3, US));
    l.take_sent();
    // A ground station starts its own download.
    l.client(wire::MSG_MISSION_REQUEST_LIST, &[FC.0, FC.1, 0]);
    l.adv(1500);
    assert!(l.take_sent().is_empty(), "aborted, no retry while the GCS transfers");
    l.adv(2000);
    assert!(matches!(l.take_sent().as_slice(), [MavMessage::MISSION_REQUEST_LIST(_)]));
}

#[test]
fn an_accepted_client_upload_is_adopted_and_counted() {
    let mut l = copter_with_mission();
    l.client(wire::MSG_MISSION_COUNT, &count_payload(3, FC));
    for seq in 0..3 {
        l.client(wire::MSG_MISSION_ITEM_INT, &item_payload(seq, FC, 16));
    }
    l.fc(wire::MSG_MISSION_ACK, &[GCS.0, GCS.1, 0, 0]);
    assert_eq!(l.core.mirror.items.len(), 3);
    assert_eq!(l.core.mirror.rev, 2);
    assert_eq!(l.core.tracker.uploads_seen(), 1);
}

// -- mission state, ArduPilot 4.5+ (mission_state present) -------------------

#[test]
fn rows_with_mission_state() {
    let mut l = copter_with_mission();
    l.current(0, 0, 1);
    assert_eq!(l.state(), MissionState::None);
    l.current(1, 4, 2);
    assert_eq!(l.state(), MissionState::Loaded);
    assert!(l.core.status(l.t).from_mission_state);
    l.heartbeat(2, 3, true); // AUTO, armed
    l.current(1, 4, 3);
    assert_eq!(l.state(), MissionState::Active);
    // A client pause, acknowledged by the FC: still AUTO, still ACTIVE.
    let mut cmd = vec![0u8; 28];
    cmd.extend_from_slice(&193u16.to_le_bytes());
    cmd.extend_from_slice(&[FC.0, FC.1, 0]);
    l.client(wire::MSG_COMMAND_LONG, &cmd);
    l.fc(wire::MSG_COMMAND_ACK, &[193, 0, 0]);
    assert_eq!(l.state(), MissionState::Paused);
    // Continue.
    let mut cont = 1f32.to_le_bytes().to_vec();
    cont.extend_from_slice(&[0u8; 24]);
    cont.extend_from_slice(&193u16.to_le_bytes());
    cont.extend_from_slice(&[FC.0, FC.1, 0]);
    l.client(wire::MSG_COMMAND_LONG, &cont);
    l.fc(wire::MSG_COMMAND_ACK, &[193, 0, 0]);
    assert_eq!(l.state(), MissionState::Active);
    // LOITER after AUTO: NOT_STARTED + latched + hold = paused.
    l.heartbeat(2, 5, true);
    l.current(2, 4, 2);
    assert_eq!(l.state(), MissionState::Paused);
    // RTL after the mission was active: returning.
    l.heartbeat(2, 6, true);
    assert_eq!(l.state(), MissionState::Returning);
    // COMPLETE.
    l.current(4, 4, 5);
    assert_eq!(l.state(), MissionState::Complete);
    // Disarm clears the latch.
    l.heartbeat(2, 6, false);
    l.current(0, 4, 2);
    assert_eq!(l.state(), MissionState::Loaded);
}

#[test]
fn auto_rtl_is_active_with_landing_seq() {
    let mut l = copter_with_mission();
    l.heartbeat(2, 27, true);
    l.current(3, 4, 3);
    let st = l.core.status(l.t);
    assert_eq!(st.state, MissionState::Active);
    assert!(st.landing_seq);
    assert_eq!(st.to_json()["landing_seq"], true);
}

#[test]
fn a_stale_mission_current_falls_back() {
    let mut l = copter_with_mission();
    l.heartbeat(2, 3, true);
    l.current(2, 4, 3);
    l.adv(3100);
    l.heartbeat(2, 3, true);
    let st = l.core.status(l.t);
    assert!(!st.from_mission_state);
    assert_eq!(st.seq, None);
}

// -- mission state, fallback (ArduPilot 4.4: seq only) -----------------------

#[test]
fn rows_without_mission_state() {
    let mut l = copter_with_mission();
    l.current(0, 0, 0);
    assert_eq!(l.state(), MissionState::Loaded, "mirror present, no S");
    assert!(!l.core.status(l.t).from_mission_state);
    l.heartbeat(2, 3, true);
    l.current(1, 0, 0);
    assert_eq!(l.state(), MissionState::Active);
    l.heartbeat(2, 17, true); // BRAKE
    assert_eq!(l.state(), MissionState::Paused);
    l.heartbeat(2, 21, true); // SMART_RTL
    assert_eq!(l.state(), MissionState::Returning);
    // The last item (seq 4 = ArduPilot total with home) reached.
    l.fc(wire::MSG_MISSION_ITEM_REACHED, &4u16.to_le_bytes());
    assert_eq!(l.state(), MissionState::Complete);
    l.heartbeat(2, 21, false);
    assert_eq!(l.state(), MissionState::Loaded);
}

#[test]
fn an_empty_mirror_is_none() {
    let mut l = Link::new();
    l.heartbeat(2, 3, true);
    l.fc(wire::MSG_MISSION_COUNT, &count_payload(1, GCS));
    l.fc(wire::MSG_MISSION_ITEM_INT, &item_payload(0, GCS, 16));
    l.current(0, 0, 0);
    assert_eq!(l.state(), MissionState::None);
}

// -- Plane mode numbers -----------------------------------------------------

#[test]
fn plane_modes() {
    let mut l = copter_with_mission();
    l.heartbeat(1, 10, true); // Plane AUTO
    l.current(2, 4, 3);
    assert_eq!(l.state(), MissionState::Active);
    l.heartbeat(1, 12, true); // LOITER
    l.current(2, 4, 2);
    assert_eq!(l.state(), MissionState::Paused);
    l.heartbeat(1, 11, true); // RTL
    assert_eq!(l.state(), MissionState::Returning);
    l.heartbeat(1, 3, true); // copter AUTO number means nothing on a plane
    assert_eq!(l.state(), MissionState::Loaded);
    l.heartbeat(1, 20, true); // QLAND
    assert_eq!(l.state(), MissionState::Returning);
}

#[test]
fn total_mismatch_triggers_a_download() {
    let mut l = copter_with_mission();
    l.adv(3100);
    l.take_sent();
    l.current(1, 7, 2);
    l.adv(200);
    assert!(matches!(l.take_sent().as_slice(), [MavMessage::MISSION_REQUEST_LIST(_)]));
}
