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
    assert!(matches!(
        sent.as_slice(),
        [MavMessage::MISSION_REQUEST_LIST(_)]
    ));
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
    assert!(matches!(
        l.take_sent().as_slice(),
        [MavMessage::MISSION_ACK(_)]
    ));
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
        assert!(matches!(
            l.take_sent().as_slice(),
            [MavMessage::MISSION_REQUEST_LIST(_)]
        ));
    }
    l.adv(1000);
    assert_eq!(l.core.downloads_failed, 1);
    assert!(l.take_sent().is_empty());
    l.adv(1000);
    assert!(l.take_sent().is_empty(), "inside the 2 s backoff");
    l.adv(1100);
    assert!(matches!(
        l.take_sent().as_slice(),
        [MavMessage::MISSION_REQUEST_LIST(_)]
    ));
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
    assert!(
        l.take_sent().is_empty(),
        "aborted, no retry while the GCS transfers"
    );
    l.adv(2000);
    assert!(matches!(
        l.take_sent().as_slice(),
        [MavMessage::MISSION_REQUEST_LIST(_)]
    ));
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
    assert!(matches!(
        l.take_sent().as_slice(),
        [MavMessage::MISSION_REQUEST_LIST(_)]
    ));
}

#[test]
fn an_upload_the_router_never_saw_is_fetched_without_total() {
    // ArduPilot 4.4: MISSION_CURRENT carries seq only. The plan was uploaded
    // straight to the FC, so the mirror holds only the empty boot download.
    let mut l = Link::new();
    l.heartbeat(2, 0, false);
    l.fc(wire::MSG_MISSION_COUNT, &count_payload(1, GCS));
    l.fc(wire::MSG_MISSION_ITEM_INT, &item_payload(0, GCS, 16));
    l.adv(3100);
    l.take_sent();
    // Entering AUTO with nothing mirrored asks for the plan.
    l.heartbeat(2, 3, true);
    l.adv(200);
    assert!(matches!(
        l.take_sent().as_slice(),
        [MavMessage::MISSION_REQUEST_LIST(_)]
    ));
}

#[test]
fn a_seq_past_the_mirror_triggers_one_download() {
    let mut l = copter_with_mission();
    l.adv(3100);
    l.take_sent();
    l.current(9, 0, 0);
    l.adv(200);
    assert!(matches!(
        l.take_sent().as_slice(),
        [MavMessage::MISSION_REQUEST_LIST(_)]
    ));
    // The same seq again does not ask twice.
    l.fc(wire::MSG_MISSION_COUNT, &count_payload(5, US));
    for seq in 0..5 {
        l.fc(wire::MSG_MISSION_ITEM_INT, &item_payload(seq, US, 16));
    }
    l.take_sent();
    l.current(9, 0, 0);
    l.adv(200);
    assert!(l.take_sent().is_empty());
}

// -- target -----------------------------------------------------------------

const VLAT: f64 = 47.0;
const VLON: f64 = 8.5;

/// `(cmd, frame, lat_e7, lon_e7, alt_m, param3)` of one item.
type Spot = (u16, u8, i32, i32, f32, f32);

fn e7(deg: f64) -> i32 {
    (deg * 1e7).round() as i32
}

/// The item `n` metres north, `e` metres east of the vehicle's fix.
fn spot(cmd: u16, frame: u8, n: f64, e: f64, alt: f32) -> Spot {
    let lat = VLAT + n * ados_swarm::geo::M_TO_DEG;
    let lon = VLON + e * ados_swarm::geo::M_TO_DEG / VLAT.to_radians().cos();
    (cmd, frame, e7(lat), e7(lon), alt, 0.0)
}

fn spot_payload(seq: u16, (cmd, frame, lat, lon, alt, p3): Spot) -> Vec<u8> {
    let mut p = Vec::new();
    for param in [0f32, 0.0, p3, 0.0] {
        p.extend_from_slice(&param.to_le_bytes());
    }
    p.extend_from_slice(&lat.to_le_bytes());
    p.extend_from_slice(&lon.to_le_bytes());
    p.extend_from_slice(&alt.to_le_bytes());
    p.extend_from_slice(&seq.to_le_bytes());
    p.extend_from_slice(&cmd.to_le_bytes());
    p.extend_from_slice(&[GCS.0, GCS.1, frame, 0, 1, 0]);
    p
}

/// An armed copter in AUTO mirroring home plus `items` (seq 1 onwards).
fn flying(items: &[Spot]) -> Link {
    let mut l = Link::new();
    l.heartbeat(2, 3, true);
    let count = items.len() as u16 + 1;
    l.fc(wire::MSG_MISSION_COUNT, &count_payload(count, GCS));
    let home = (16, 0, e7(VLAT), e7(VLON), 500.0, 0.0);
    for (seq, s) in std::iter::once(home)
        .chain(items.iter().copied())
        .enumerate()
    {
        l.fc(wire::MSG_MISSION_ITEM_INT, &spot_payload(seq as u16, s));
    }
    assert_eq!(l.core.mirror.items.len(), usize::from(count));
    l
}

/// The vehicle at local (5, 5, -3), 3 m above home and 500 m AMSL, sampled now.
fn vehicle_here(l: &mut Link) {
    let at = l.t;
    l.core.set_position(VehiclePosition {
        global: Some(GlobalFix {
            lat_deg: VLAT,
            lon_deg: VLON,
            alt_msl_m: 500.0,
            alt_rel_m: 3.0,
            at,
        }),
        local: Some(([5.0, 5.0, -3.0], at)),
    });
}

/// The `target` of the snapshot with MISSION_CURRENT at `seq`.
fn target_at(l: &mut Link, seq: u16) -> Value {
    let total = l.core.mirror.items.len() as u16 - 1;
    l.current(seq, total, 3);
    let t = l.t;
    l.core
        .state_json(t)
        .get("target")
        .cloned()
        .unwrap_or(Value::Null)
}

fn assert_ned(target: &Value, want: [f64; 3]) {
    let got: Vec<f64> = target["ned"]
        .as_array()
        .unwrap_or_else(|| panic!("no ned in {target}"))
        .iter()
        .map(|v| v.as_f64().unwrap())
        .collect();
    for (g, w) in got.iter().zip(want) {
        assert!((g - w).abs() < 0.1, "ned {got:?}, want {want:?}");
    }
}

#[test]
fn the_target_is_the_current_item_in_the_local_frame() {
    let mut l = flying(&[
        spot(16, 6, 20.0, 10.0, 10.0),
        spot(16, 5, 20.0, 10.0, 510.0),
    ]);
    vehicle_here(&mut l);
    // Relative altitude: 10 m above home with the vehicle 3 m above it.
    let t = target_at(&mut l, 1);
    assert_eq!((t["seq"].as_u64(), t["cmd"].as_u64()), (Some(1), Some(16)));
    assert!(t.get("radius_m").is_none());
    assert_ned(&t, [25.0, 15.0, -10.0]);
    let typed: ados_protocol::pilot::MissionTarget = serde_json::from_value(t).unwrap();
    assert_eq!((typed.seq, typed.cmd, typed.radius_m), (1, 16, None));
    // AMSL: 510 m with the vehicle at 500 m.
    assert_ned(&target_at(&mut l, 2), [25.0, 15.0, -13.0]);
}

#[test]
fn items_without_a_position_have_a_null_ned() {
    let mut l = flying(&[
        spot(93, 6, 20.0, 10.0, 10.0),
        spot(16, 11, 20.0, 10.0, 10.0),
        (16, 6, 0, 0, 10.0, 0.0),
        spot(16, 1, 20.0, 10.0, 10.0),
    ]);
    vehicle_here(&mut l);
    // DELAY, a terrain frame, a zero position (here) and a local frame.
    for (seq, cmd) in [(1, 93), (2, 16), (3, 16), (4, 16)] {
        let t = target_at(&mut l, seq);
        assert_eq!(t["cmd"], cmd, "seq {seq}: {t}");
        assert!(t["ned"].is_null(), "seq {seq}: {t}");
    }
}

#[test]
fn no_fresh_vehicle_position_gives_a_null_ned() {
    let mut l = flying(&[spot(16, 6, 20.0, 10.0, 10.0)]);
    assert!(target_at(&mut l, 1)["ned"].is_null());
    // A global fix alone is not enough.
    let at = l.t;
    l.core.set_position(VehiclePosition {
        global: Some(GlobalFix {
            lat_deg: VLAT,
            lon_deg: VLON,
            alt_msl_m: 500.0,
            alt_rel_m: 3.0,
            at,
        }),
        local: None,
    });
    assert!(target_at(&mut l, 1)["ned"].is_null());
    // Both, then left to go stale.
    vehicle_here(&mut l);
    assert_ned(&target_at(&mut l, 1), [25.0, 15.0, -10.0]);
    l.adv(1500);
    assert!(target_at(&mut l, 1)["ned"].is_null());
}

#[test]
fn a_seq_without_a_mirrored_item_omits_the_target() {
    let mut l = flying(&[spot(16, 6, 20.0, 10.0, 10.0)]);
    vehicle_here(&mut l);
    assert!(target_at(&mut l, 5).is_null());
    // ArduPilot's home slot is not a flown item.
    assert!(target_at(&mut l, 0).is_null());
    assert!(!target_at(&mut l, 1).is_null());
}

#[test]
fn loiter_turns_and_to_alt_carry_their_radius() {
    let with_p3 = |s: Spot, p3: f32| (s.0, s.1, s.2, s.3, s.4, p3);
    let mut l = flying(&[
        with_p3(spot(18, 6, 20.0, 10.0, 10.0), -30.0),
        with_p3(spot(31, 6, 20.0, 10.0, 10.0), 25.0),
        with_p3(spot(19, 6, 20.0, 10.0, 10.0), 40.0),
        spot(18, 6, 20.0, 10.0, 10.0),
    ]);
    vehicle_here(&mut l);
    assert_eq!(target_at(&mut l, 1)["radius_m"], 30.0);
    let to_alt = target_at(&mut l, 2);
    assert_eq!(to_alt["radius_m"], 25.0);
    assert_ned(&to_alt, [25.0, 15.0, -10.0]);
    assert!(target_at(&mut l, 3).get("radius_m").is_none());
    assert!(target_at(&mut l, 4).get("radius_m").is_none());
}
