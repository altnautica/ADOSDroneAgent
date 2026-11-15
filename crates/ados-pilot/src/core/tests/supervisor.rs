//! The supervisor over FC-flown missions and returns: the proximity brake in
//! AUTO, the no-progress rule with its operator actions and automatic skip,
//! the blocked return's landing, the SmartRTL return with its land fallback,
//! and the announced low-battery countdown.

use std::io::Write;
use std::sync::{Arc, Mutex};

use ados_protocol::mavlink::ardupilotmega::{MavCmd, MavMessage, MavResult};
use ados_protocol::pilot::{MissionGap, Owner};
use serde_json::{json, Value};

use super::harness::World;
use super::safety::wall_scan;
use crate::core::Reply;
use crate::modes::copter;

fn ok(r: &Reply) -> bool {
    matches!(r, Reply::Action(a) if a.ok)
}

#[derive(Clone, Default)]
struct LogBuf(Arc<Mutex<Vec<u8>>>);

impl Write for LogBuf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuf {
    type Writer = LogBuf;
    fn make_writer(&'a self) -> LogBuf {
        self.clone()
    }
}

/// Run `f` with the pilot's log lines captured.
pub(super) fn with_logs<T>(f: impl FnOnce() -> T) -> (T, String) {
    let buf = LogBuf::default();
    let sub = tracing_subscriber::fmt()
        .with_writer(buf.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    let r = tracing::subscriber::with_default(sub, f);
    let text = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    (r, text)
}

/// The router's mission state with the active item's target.
fn mission_target(w: &mut World, seq: u32, total: u32, cmd: u16, ned: Option<[f64; 3]>) {
    let snapshot = json!({
        "armed": w.armed,
        "mission": {"state": "active", "seq": seq, "total": total, "item_reached": seq.checked_sub(1),
                    "id": null, "source": "mission_state", "age_ms": 100, "landing_seq": false,
                    "target": {"seq": seq, "cmd": cmd, "ned": ned},
                    "rev": 1, "uploads_seen": 0, "items": []},
    });
    let m = crate::router_state::mission_of(snapshot);
    w.p.on_feed(crate::router_state::MISSION_TOPIC, &m, w.now);
    w.collect();
}

/// Airborne in AUTO at (0, 0, -5), flying waypoint `seq` of 8 that lies 20 m
/// north, with the vehicle making no progress.
fn stuck_in_auto(seq: u32) -> World {
    let mut w = World::new();
    w.airborne(copter::AUTO);
    assert_eq!(w.p.owner(), Owner::MissionFc);
    mission_target(&mut w, seq, 8, 16, Some([20.0, 0.0, -5.0]));
    w
}

/// The items the pilot made current (MAV_CMD_DO_SET_MISSION_CURRENT).
fn set_currents(w: &World) -> Vec<u16> {
    w.sent
        .iter()
        .filter_map(|m| match m {
            MavMessage::COMMAND_LONG(c) if c.command == MavCmd::MAV_CMD_DO_SET_MISSION_CURRENT => {
                Some(c.param1 as u16)
            }
            _ => None,
        })
        .collect()
}

fn banner_key(w: &World) -> Option<String> {
    w.p.state(w.now).banner.map(|b| b.text_key)
}

#[test]
fn a_stalled_mission_alerts_holds_and_skips_after_the_countdown() {
    let ((), logs) = with_logs(|| {
        let mut w = stuck_in_auto(3);
        let t0 = w.now;
        w.run(14_800);
        assert!(
            w.p.state(w.now).mission_stall.is_none(),
            "no stall before 15 s"
        );
        assert_eq!(w.set_modes(copter::BRAKE), 0);
        w.run(400);
        let s = w.p.state(w.now);
        let stall = s.mission_stall.expect("a stall at 15 s");
        assert_eq!(stall.kind, "mission");
        assert_eq!(stall.seq, Some(3));
        assert_eq!(stall.wp_label.as_deref(), Some("3"));
        assert_eq!(stall.window_s, 15.0);
        let in_s = stall.skip_in_s.expect("a countdown");
        assert!((44.5..=45.0).contains(&in_s), "{in_s}");
        let raised = w.now + (in_s * 1000.0).round() as u64 - 45_000 - t0;
        assert!((14_800..=15_200).contains(&raised), "raised at {raised} ms");
        assert_eq!(
            banner_key(&w).as_deref(),
            Some("pilot.banner.mission_stalled")
        );
        // BRAKE, then the GUIDED hold as `pilot`.
        assert_eq!(w.set_modes(copter::BRAKE), 1);
        assert_eq!(w.mode, copter::GUIDED);
        assert_eq!(w.p.owner(), Owner::Pilot);
        assert_eq!(s.hold_reason.as_deref(), Some("mission-stalled"));
        // The countdown runs to the automatic skip at 60 s.
        let skip_at = t0 + 60_000;
        w.run(skip_at - 300 - w.now);
        assert!(
            set_currents(&w).is_empty(),
            "no skip before the countdown ends"
        );
        w.run(600);
        assert_eq!(set_currents(&w), vec![4]);
        assert_eq!(w.mode, copter::AUTO);
        assert_eq!(w.p.owner(), Owner::MissionFc);
        let s = w.p.state(w.now);
        assert!(s.mission_stall.is_none());
        assert_eq!(
            s.mission.unwrap().gaps,
            vec![MissionGap {
                seq: 3,
                reason: "stall_timeout".into(),
                at_ms: s.banner.as_ref().unwrap().until_ms - crate::flows::BANNER_MS
            }]
        );
        assert_eq!(
            banner_key(&w).as_deref(),
            Some("pilot.banner.mission_wp_skipped")
        );
    });
    assert!(logs.contains("pilot_mission_stall"), "{logs}");
    assert!(logs.contains("pilot_mission_skip"), "{logs}");
    assert!(logs.contains("pilot_mode_command"), "{logs}");
}

/// Stalled at 15 s, then answered.
fn stalled() -> World {
    let mut w = stuck_in_auto(3);
    w.run(15_200);
    assert!(w.p.state(w.now).mission_stall.is_some());
    w
}

#[test]
fn the_operator_answers_a_stall_at_once_and_the_countdown_ends() {
    let mut w = stalled();
    assert!(ok(&w.action("mission_skip")));
    assert_eq!(set_currents(&w), vec![4]);
    w.run(100);
    assert_eq!(w.mode, copter::AUTO);
    let s = w.p.state(w.now);
    assert!(s.mission_stall.is_none());
    assert_eq!(s.mission.unwrap().gaps[0].reason, "stall_operator");

    let mut w = stalled();
    assert!(ok(&w.action("rth")));
    w.run(100);
    assert_eq!(w.mode, copter::SMART_RTL);
    assert!(w.p.state(w.now).mission_stall.is_none());
    w.run(50_000);
    assert!(set_currents(&w).is_empty());

    let mut w = stalled();
    assert!(ok(&w.action("land")));
    w.run(100);
    assert!(w.p.state(w.now).mission_stall.is_none());
    w.run(50_000);
    assert!(set_currents(&w).is_empty());
    assert_ne!(w.mode, copter::AUTO);

    // A stick takeover ends the countdown; the operator flies and the
    // mission waits for `resume`.
    let mut w = stalled();
    w.input([0.0; 4]);
    w.stick([0.6, 0.0, 0.0, 0.0], 200);
    let s = w.p.state(w.now);
    assert!(s.mission_stall.is_none());
    assert_eq!(w.p.owner(), Owner::Pilot);
    assert_eq!(s.mission.unwrap().state, "paused");
    w.stick([0.0; 4], 50_000);
    assert!(set_currents(&w).is_empty());
}

#[test]
fn two_automatic_skips_in_a_row_then_the_stall_holds_for_the_operator() {
    let mut w = stuck_in_auto(3);
    w.run(60_400);
    assert_eq!(set_currents(&w), vec![4]);
    mission_target(&mut w, 4, 8, 16, Some([20.0, 0.0, -5.0]));
    w.run(60_400);
    assert_eq!(set_currents(&w), vec![4, 5]);
    mission_target(&mut w, 5, 8, 16, Some([20.0, 0.0, -5.0]));
    w.run(15_400);
    let stall = w.p.state(w.now).mission_stall.expect("a third stall");
    assert_eq!(stall.seq, Some(5));
    assert_eq!(
        stall.skip_in_s, None,
        "no countdown after two automatic skips"
    );
    assert_eq!(
        banner_key(&w).as_deref(),
        Some("pilot.banner.mission_stalled_hold")
    );
    w.run(120_000);
    assert_eq!(set_currents(&w), vec![4, 5], "it holds for the operator");
    assert_eq!(w.p.owner(), Owner::Pilot);
    assert_eq!(w.mode, copter::GUIDED);
    // The operator's skip starts the count again.
    assert!(ok(&w.action("mission_skip")));
    assert_eq!(set_currents(&w), vec![4, 5, 6]);
}

#[test]
fn delay_and_loiter_items_never_stall() {
    for (cmd, ned) in [
        (93u16, None),
        (19, Some([0.5, 0.0, -5.0])),
        (17, Some([1.0, 0.0, -5.0])),
    ] {
        let mut w = World::new();
        w.airborne(copter::AUTO);
        mission_target(&mut w, 2, 4, cmd, ned);
        w.run(70_000);
        assert!(w.p.state(w.now).mission_stall.is_none(), "cmd {cmd}");
        assert_eq!(w.set_modes(copter::BRAKE), 0, "cmd {cmd}");
        assert_eq!(w.mode, copter::AUTO);
    }
}

#[test]
fn a_blocked_return_alerts_then_lands_in_place() {
    let mut w = World::new();
    w.airborne(copter::SMART_RTL);
    let t0 = w.now;
    w.run(15_300);
    let s = w.p.state(w.now);
    let stall = s.mission_stall.expect("a blocked return");
    assert_eq!(stall.kind, "return");
    assert_eq!(stall.seq, None);
    assert_eq!(s.hold_reason.as_deref(), Some("return-blocked"));
    assert_eq!(
        banner_key(&w).as_deref(),
        Some("pilot.banner.return_blocked")
    );
    assert_eq!(w.mode, copter::GUIDED);
    w.run(t0 + 59_800 - w.now);
    assert_eq!(w.set_modes(copter::LAND), 0);
    w.run(600);
    assert_eq!(w.set_modes(copter::LAND), 1);
    assert_eq!(w.mode, copter::LAND);
    assert!(w.p.state(w.now).mission_stall.is_none());
}

#[test]
fn the_return_is_smart_rtl_and_lands_when_the_fc_refuses_it() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    assert!(ok(&w.action("rth")));
    w.run(1500);
    assert_eq!(w.set_modes(copter::SMART_RTL), 1);
    assert_eq!(w.set_modes(copter::RTL), 0, "never a straight RTL");
    assert_eq!(w.mode, copter::SMART_RTL);
    assert_eq!(w.set_modes(copter::LAND), 0);

    // A FAILED ack: land at once.
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.acks = true;
    w.refuse_mode = Some((copter::SMART_RTL, MavResult::MAV_RESULT_FAILED, None));
    let ((), logs) = with_logs(|| {
        assert!(ok(&w.action("rth")));
        w.run(100);
    });
    assert_eq!(w.set_modes(copter::LAND), 1);
    assert_eq!(w.mode, copter::LAND);
    assert_eq!(w.set_modes(copter::RTL), 0);
    assert_eq!(
        banner_key(&w).as_deref(),
        Some("pilot.banner.return_land_fallback")
    );
    assert!(logs.contains("pilot_return"), "{logs}");

    // No answer and no mode change within 1 s: land.
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.refuse_mode = Some((copter::SMART_RTL, MavResult::MAV_RESULT_FAILED, None));
    assert!(ok(&w.action("rth")));
    w.run(900);
    assert_eq!(w.set_modes(copter::LAND), 0);
    w.run(200);
    assert_eq!(w.set_modes(copter::LAND), 1);
}

fn scan_for(w: &mut World, scan: &Value, ms: u64) {
    let end = w.now + ms;
    while w.now < end {
        w.p.on_feed("plugin.world-engine.scan", scan, w.now);
        w.run(20);
    }
}

#[test]
fn an_obstacle_ahead_in_auto_brakes_within_one_replan_and_resumes_once_clear() {
    let mut w = World::new();
    w.airborne(copter::AUTO);
    mission_target(&mut w, 2, 4, 16, Some([30.0, 0.0, -5.0]));
    w.vel = [3.0, 0.0, 0.0];
    let ((), logs) = with_logs(|| scan_for(&mut w, &wall_scan(2.0), 220));
    assert_eq!(w.set_modes(copter::BRAKE), 1);
    assert_eq!(
        banner_key(&w).as_deref(),
        Some("pilot.banner.obstacle_close")
    );
    assert!(logs.contains("pilot_auto_brake"), "{logs}");
    w.vel = [0.0; 3];
    scan_for(&mut w, &wall_scan(2.0), 500);
    assert_eq!(w.mode, copter::GUIDED);
    assert_eq!(w.p.owner(), Owner::Pilot);
    // The wall stays: the hold stays.
    scan_for(&mut w, &wall_scan(2.0), 4000);
    assert_eq!(w.mode, copter::GUIDED);
    // Clear for 3 s: the mission resumes in AUTO from the same item.
    w.run(2900);
    assert_eq!(w.mode, copter::GUIDED);
    w.run(400);
    assert_eq!(w.mode, copter::AUTO);
    assert_eq!(w.p.owner(), Owner::MissionFc);
    assert!(set_currents(&w).is_empty());
}

#[test]
fn the_low_battery_countdown_is_announced_and_logged() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    assert_eq!(w.p.owner(), Owner::Pilot);
    let ((), logs) = with_logs(|| {
        w.p.fc.battery_pct = Some(15.0);
        w.run(100);
    });
    let s = w.p.state(w.now);
    let b = s.banner.expect("the countdown banner");
    assert_eq!(b.text_key, "pilot.banner.low_battery_rth");
    assert_eq!(b.args["pct"], json!(15));
    assert_eq!(b.args["need_pct"], json!(20));
    assert_eq!(b.args["in_s"], json!(10));
    assert!(s.rth_countdown_s.is_some());
    assert!(
        logs.contains("pilot_battery_reserve") && logs.contains("countdown"),
        "{logs}"
    );
    let ((), logs) = with_logs(|| {
        w.p.fc.battery_pct = Some(15.0);
        w.run(10_100);
    });
    assert_eq!(w.mode, copter::SMART_RTL);
    assert!(logs.contains("fired"), "{logs}");
}

#[test]
fn the_golden_stall_frame_round_trips() {
    let text = include_str!("../../../../ados-control/tests/fixtures/pilot-frame-stall.json");
    let v: Value = serde_json::from_str(text).unwrap();
    let s: ados_protocol::pilot::PilotState = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(serde_json::to_value(&s).unwrap(), v);
    assert_eq!(s.mission_stall.unwrap().skip_in_s, Some(32.5));
    assert_eq!(s.mission.unwrap().gaps.len(), 1);
}

fn api_action(w: &mut World, name: &str) -> Reply {
    w.req(ados_protocol::pilot::Request::Action(
        ados_protocol::pilot::ActionReq {
            name: name.into(),
            src: "api".into(),
            args: None,
            id: None,
        },
    ))
}

#[test]
fn api_commands_neither_start_nor_hold_off_the_link_loss_timer() {
    // A one-shot `/api/command` action with no surface link: no link-loss
    // countdown, and the vehicle keeps its hold.
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    assert!(ok(&api_action(&mut w, "take_control")));
    w.run(12_000);
    assert_eq!(w.p.state(w.now).link_loss_in_s, None);
    assert_eq!(w.mode, copter::GUIDED);
    assert_eq!(w.p.owner(), Owner::Pilot);

    // A surface that goes quiet still loses its link on time, whatever
    // `/api/command` sends meanwhile.
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.action("take_control");
    w.req(ados_protocol::pilot::Request::Hb { src: "mc".into() });
    for _ in 0..5 {
        w.run(2000);
        let _ = api_action(&mut w, "rth_cancel");
    }
    w.run(200);
    assert_eq!(w.mode, copter::SMART_RTL, "the link loss fired at 10 s");
}
