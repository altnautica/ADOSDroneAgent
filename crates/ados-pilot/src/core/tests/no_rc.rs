//! Flight with no RC transmitter (`pilot.rc_receiver: absent`): the link-loss
//! watch covers every airborne owner and each operator instance (`sid`) on
//! its own, a `bye` ends an instance's watch only while disarmed or after a
//! takeover, a follow never restarts into a return, and the declaration is
//! taken only where its posture holds.

use ados_protocol::pilot::{
    ActionReq, Holder, LeaseReply, Owner, PlaceClass, RcReceiver, Request, Severity,
    CLIENT_MODE_TOPIC,
};
use serde_json::json;

use super::harness::World;
use crate::core::Reply;
use crate::modes::copter;

const PHONE: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f0";
const PHONE_2: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90";
const TAB: &str = "5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a";

/// A request from surface instance `sid`.
fn from(w: &mut World, sid: &str, r: Request) -> Reply {
    let rep = w.p.on_request_from(r, Some(sid), w.now);
    w.collect();
    rep
}

fn hb(w: &mut World, sid: &str, src: &str) {
    from(w, sid, Request::Hb { src: src.into() });
}

fn act(w: &mut World, sid: &str, src: &str, name: &str) -> Reply {
    from(
        w,
        sid,
        Request::Action(ActionReq {
            name: name.into(),
            src: src.into(),
            args: None,
            id: None,
        }),
    )
}

fn err(r: &Reply) -> Option<&str> {
    match r {
        Reply::Action(a) if !a.ok => a.err.as_deref(),
        _ => None,
    }
}

/// Run `ms`, with each of `live` sending `hb` every 500 ms.
fn run_with(w: &mut World, ms: u64, live: &[(&str, &str)]) {
    let end = w.now + ms;
    while w.now < end {
        w.run(500);
        for (sid, src) in live {
            hb(w, sid, src);
        }
    }
}

/// Declared with no RC with the FC's no-RC posture (RC failsafe off, the
/// fence), on the ground.
fn grounded() -> World {
    let mut w = World::new();
    w.p.settings.pilot.rc_receiver = RcReceiver::Absent;
    w.p.fc.params.insert("FS_THR_ENABLE".into(), 0.0);
    fence(&mut w);
    w
}

/// [`grounded`], then airborne in GUIDED, flown by the phone's pilot.
fn flying() -> World {
    let mut w = grounded();
    w.airborne(copter::GUIDED);
    hb(&mut w, PHONE, "android");
    let r = act(&mut w, PHONE, "android", "take_control");
    assert_eq!(err(&r), None, "{r:?}");
    w.run(100);
    assert_eq!(w.p.owner(), Owner::Pilot);
    w.sent.clear();
    w
}

/// Surfaces that went quiet before arming (a tab reloaded under a new
/// `sid`, a closed dashboard sending no `sid`) are not watched in the
/// flight: no return right after take-off. The phone, quiet only for the
/// second before arming, still is.
#[test]
fn surfaces_quiet_before_arming_are_not_watched() {
    let mut w = grounded();
    hb(&mut w, TAB, "mc");
    w.p.on_request_from(
        Request::Hb {
            src: "dashboard".into(),
        },
        None,
        w.now,
    );
    w.run(20_000);
    hb(&mut w, PHONE, "android");
    w.run(1_000);
    w.airborne(copter::GUIDED);
    run_with(&mut w, 12_000, &[(PHONE, "android")]);
    assert!(!returned(&w), "mode {}", w.mode);
    assert_eq!(w.p.failsafe.instances.unrecovered(), None);

    let mut w = grounded();
    hb(&mut w, PHONE, "android");
    w.run(1_000);
    w.airborne(copter::GUIDED);
    w.run(10_500);
    assert!(returned(&w), "the phone was live at arming: mode {}", w.mode);
}

fn returned(w: &World) -> bool {
    w.mode == copter::SMART_RTL
}

fn lease_reply(w: &mut World) -> LeaseReply {
    match w.req(Request::LeaseAcquire {
        holder: Holder {
            plugin_id: "com.example.follow".into(),
            skill_id: "follow-me".into(),
        },
        kind: "motion".into(),
    }) {
        Reply::Lease(l) => l,
        other => panic!("{other:?}"),
    }
}

/// The app's MAVLink fallback Pause puts the FC in BRAKE (owner `fc_mode`);
/// the phone then goes silent → the link-loss action fires. With an RC
/// transmitter only the pilot's own owners are watched, so nothing fired.
#[test]
fn silence_while_the_fc_owns_brake_returns() {
    let mut w = flying();
    w.p.on_feed(CLIENT_MODE_TOPIC, &json!({ "mode": copter::BRAKE }), w.now);
    w.mode = copter::BRAKE;
    w.run(200);
    assert_eq!(w.p.owner(), Owner::FcMode);
    assert!(!w.p.state(w.now).fc_failsafe);
    w.run(9_000);
    assert_eq!(w.mode, copter::BRAKE, "not before link_loss_s");
    w.run(1_500);
    assert!(returned(&w), "mode {}", w.mode);
}

/// Hand to FC (Loiter) stays in ADOS control with no RC, and silence there
/// returns too.
#[test]
fn silence_after_hand_to_fc_returns_and_ados_control_stays() {
    let mut w = flying();
    act(&mut w, PHONE, "android", "hand_to_fc");
    w.run(200);
    assert_eq!(w.mode, copter::LOITER);
    assert_eq!(w.p.control_mode(), ados_protocol::pilot::ControlMode::Ados);
    w.run(11_000);
    assert!(returned(&w), "mode {}", w.mode);
}

/// The FC's battery failsafe lands: silence never turns the landing into a
/// return.
#[test]
fn an_fc_battery_landing_is_left_alone() {
    let mut w = flying();
    w.mode = copter::LAND;
    w.run(200);
    assert!(w.p.state(w.now).fc_failsafe);
    w.run(15_000);
    assert_eq!(w.mode, copter::LAND);
    assert_eq!(w.set_modes(copter::SMART_RTL), 0);
}

/// A tab that only sends `hb` never hides the phone's loss.
#[test]
fn a_second_live_instance_does_not_hide_the_phone() {
    let mut w = flying();
    run_with(&mut w, 9_500, &[(TAB, "mc")]);
    assert!(!returned(&w));
    run_with(&mut w, 1_500, &[(TAB, "mc")]);
    assert!(returned(&w), "mode {}", w.mode);
}

/// Two phones share `src: android`; one going silent fires.
#[test]
fn two_phones_with_one_src_are_watched_apart() {
    let mut w = flying();
    hb(&mut w, PHONE_2, "android");
    run_with(&mut w, 11_000, &[(PHONE_2, "android")]);
    assert!(returned(&w), "mode {}", w.mode);
}

/// The same instance back on a new connection inside the window continues
/// its own timer: no link loss.
#[test]
fn a_reconnect_inside_the_window_is_no_loss() {
    let mut w = flying();
    w.run(8_000);
    run_with(&mut w, 10_000, &[(PHONE, "android")]);
    assert_eq!(w.mode, copter::GUIDED);
    assert_eq!(w.p.owner(), Owner::Pilot);
}

/// A `bye` while disarmed ends the watch: the next flight watches only those
/// that speak after it, and an instance that left is not among them.
#[test]
fn bye_while_disarmed_is_honoured() {
    let mut w = World::new();
    w.p.settings.pilot.rc_receiver = RcReceiver::Absent;
    hb(&mut w, PHONE, "android");
    from(
        &mut w,
        PHONE,
        Request::Bye {
            src: "android".into(),
        },
    );
    w.airborne(copter::GUIDED);
    w.run(15_000);
    assert_eq!(w.mode, copter::GUIDED);
    assert_eq!(w.p.failsafe.instances.unrecovered(), None);
}

/// The last instance saying `bye` in flight counts as silence.
#[test]
fn the_last_bye_in_flight_still_returns() {
    let mut w = flying();
    from(
        &mut w,
        PHONE,
        Request::Bye {
            src: "android".into(),
        },
    );
    w.run(9_000);
    assert!(!returned(&w));
    w.run(1_500);
    assert!(returned(&w), "mode {}", w.mode);
}

/// A `bye` beside a tab that only sends `hb` is silence; after the tab took
/// the stick session it is honoured.
#[test]
fn a_bye_is_honoured_only_after_a_takeover() {
    let mut w = flying();
    hb(&mut w, TAB, "mc");
    from(
        &mut w,
        PHONE,
        Request::Bye {
            src: "android".into(),
        },
    );
    run_with(&mut w, 11_000, &[(TAB, "mc")]);
    assert!(returned(&w), "an unattended tab is no takeover");

    let mut w = flying();
    hb(&mut w, TAB, "mc");
    from(
        &mut w,
        TAB,
        Request::SessionTake {
            src: "mc".into(),
            session: 4,
        },
    );
    from(
        &mut w,
        PHONE,
        Request::Bye {
            src: "android".into(),
        },
    );
    run_with(&mut w, 15_000, &[(TAB, "mc")]);
    assert!(!returned(&w), "mode {}", w.mode);
    assert_eq!(w.p.failsafe.instances.unrecovered(), None);
}

/// A tab that fired first never masks the phone's own silence later, and the
/// phone's frames never clear the tab's state.
#[test]
fn fired_state_is_kept_per_instance() {
    let mut w = flying();
    hb(&mut w, TAB, "mc");
    run_with(&mut w, 11_000, &[(PHONE, "android")]);
    assert!(returned(&w), "the tab fired");
    assert_eq!(w.p.failsafe.instances.unrecovered(), Some("mc"));
    // The operator takes the vehicle back from the return.
    act(&mut w, PHONE, "android", "take_control");
    w.run(200);
    assert_eq!(w.mode, copter::GUIDED);
    run_with(&mut w, 2_000, &[(PHONE, "android")]);
    assert_eq!(w.p.failsafe.instances.unrecovered(), Some("mc"));
    w.sent.clear();
    w.run(11_000);
    assert!(returned(&w), "the phone's own loss fires: mode {}", w.mode);
}

/// A follow asks for the lease during the link-loss return: refused, and the
/// FC keeps flying SmartRTL. With an RC transmitter the same request was
/// granted and switched the FC to GUIDED.
#[test]
fn no_follow_starts_into_the_link_loss_return() {
    let mut w = flying();
    w.run(11_000);
    assert!(returned(&w));
    let l = lease_reply(&mut w);
    assert!(l.lease_id.is_none(), "{l:?}");
    w.run(500);
    assert_eq!(w.mode, copter::SMART_RTL);
    assert_eq!(w.set_modes(copter::GUIDED), 0);
}

/// The operator's own return (no instance lost): a skill asking for the
/// lease is refused `failsafe` while the FC flies it, with an RC
/// transmitter or without; a GUIDED switch would cancel the return.
#[test]
fn no_skill_starts_into_an_operator_return() {
    for rc in [RcReceiver::Present, RcReceiver::Absent] {
        let mut w = grounded();
        w.p.settings.pilot.rc_receiver = rc;
        w.airborne(copter::GUIDED);
        w.action("rth");
        w.run(500);
        assert!(returned(&w), "{rc:?}: mode {}", w.mode);
        assert!(!w.p.state(w.now).fc_failsafe, "{rc:?}");
        let l = lease_reply(&mut w);
        assert_eq!(l.err.as_deref(), Some("failsafe"), "{rc:?}: {l:?}");
        assert_eq!(w.set_modes(copter::GUIDED), 0, "{rc:?}");
    }
}

/// A lost instance blocks a new follow until it speaks again.
#[test]
fn an_unrecovered_loss_refuses_the_lease_until_it_speaks() {
    let mut w = flying();
    hb(&mut w, TAB, "mc");
    run_with(&mut w, 11_000, &[(PHONE, "android")]);
    act(&mut w, PHONE, "android", "take_control");
    w.run(200);
    let l = lease_reply(&mut w);
    assert_eq!(l.err.as_deref(), Some("link_loss_unrecovered"), "{l:?}");
    hb(&mut w, TAB, "mc");
    let l = lease_reply(&mut w);
    assert!(l.lease_id.is_some(), "{l:?}");
}

fn items(w: &World) -> Vec<ados_protocol::pilot::Readiness> {
    w.p.readiness(w.now)
}

fn item(w: &World, id: &str) -> Option<ados_protocol::pilot::Readiness> {
    items(w).into_iter().find(|r| r.id == id)
}

fn fence(w: &mut World) {
    for (n, v) in [
        ("FENCE_ENABLE", 1.0),
        ("FENCE_TYPE", 3.0),
        ("FENCE_RADIUS", 70.0),
        ("FENCE_ALT_MAX", 50.0),
        ("FENCE_ACTION", 5.0),
    ] {
        w.p.fc.params.insert(n.into(), v);
    }
}

/// `absent` wants the RC failsafe off and the fence; `present` wants it on.
#[test]
fn the_profile_follows_the_declaration_both_ways() {
    let mut w = World::new();
    assert!(
        item(&w, "fc_profile").unwrap().ok,
        "present, FS_THR_ENABLE 5"
    );
    w.p.settings.pilot.rc_receiver = RcReceiver::Absent;
    let p = item(&w, "fc_profile").unwrap();
    assert!(!p.ok && p.msg.contains("FS_THR_ENABLE 5 (want 0)"), "{p:?}");
    w.p.fc.params.insert("FS_THR_ENABLE".into(), 0.0);
    let p = item(&w, "fc_profile").unwrap();
    assert!(
        !p.ok && p.msg.contains("FENCE_ENABLE absent"),
        "no fence: {p:?}"
    );
    fence(&mut w);
    assert!(item(&w, "fc_profile").unwrap().ok);
    w.p.settings.pilot.rc_receiver = RcReceiver::Present;
    let p = item(&w, "fc_profile").unwrap();
    assert!(!p.ok && p.msg.contains("FS_THR_ENABLE 0 (want 5)"), "{p:?}");
}

/// The posture rows: a Warn line, RC seen blocks, hold blocks, a long wait
/// warns; none of them with an RC transmitter.
#[test]
fn the_readiness_rows_of_the_posture() {
    let mut w = World::new();
    assert!(item(&w, "rc_receiver").is_none());
    w.p.settings.pilot.rc_receiver = RcReceiver::Absent;
    let posture = item(&w, "rc_receiver").unwrap();
    assert!(posture.ok && posture.severity == Severity::Warn);
    assert!(item(&w, "rc_seen").unwrap().ok);
    assert!(item(&w, "link_loss_posture").is_none());
    w.p.settings.pilot.link_loss_s = 20;
    assert_eq!(
        item(&w, "link_loss_posture").unwrap().severity,
        Severity::Warn
    );
    w.p.settings.pilot.link_loss_action = crate::config::LinkLossAction::Hold;
    let hold = item(&w, "link_loss_posture").unwrap();
    assert!(!hold.ok && hold.severity == Severity::Block);
    w.p.settings.pilot.link_loss_action = crate::config::LinkLossAction::Rth;
    w.p.settings.pilot.link_loss_s = 10;
    // A receiver feeds the FC: RC seen since boot blocks arming.
    w.p.fc.rc = Some(crate::fc::Stamped {
        v: [1500; 18],
        at_ms: w.now,
    });
    w.run(100);
    let seen = item(&w, "rc_seen").unwrap();
    assert!(!seen.ok && seen.severity == Severity::Block);
    w.p.fc.rc = None;
    w.run(100);
    assert!(!item(&w, "rc_seen").unwrap().ok, "a latch");
}

/// RC appearing in flight with no RC declared raises an alert.
#[test]
fn rc_seen_in_flight_alerts() {
    let mut w = flying();
    w.p.fc.rc = Some(crate::fc::Stamped {
        v: [1500; 18],
        at_ms: w.now,
    });
    w.run(100);
    assert_eq!(
        w.p.state(w.now).banner.map(|b| b.text_key).as_deref(),
        Some("pilot.banner.rc_seen")
    );
}

/// The declaration needs ADOS control, ArduCopter and an open place, and
/// while it holds the place stays open and the control mode `ados`.
#[test]
fn the_declaration_and_its_refusals() {
    let mut w = World::new();
    assert!(err(&w.action("rc_receiver:absent")).is_none());
    assert_eq!(w.p.rc_receiver(), RcReceiver::Absent);
    assert_eq!(
        err(&w.action("place_class:confined")),
        Some("rc_receiver_absent")
    );
    assert_eq!(
        err(&w.action("control_mode:fc_manual")),
        Some("rc_receiver_absent")
    );
    assert!(err(&w.action("control_mode:ados")).is_none());
    assert!(err(&w.action("rc_receiver:present")).is_none());

    assert!(err(&w.action("control_mode:fc_manual")).is_none());
    assert_eq!(err(&w.action("rc_receiver:absent")), Some("control_mode"));
    assert!(err(&w.action("control_mode:ados")).is_none());
    w.p.settings.pilot.place_class = PlaceClass::Confined;
    assert_eq!(err(&w.action("rc_receiver:absent")), Some("confined"));
    w.p.settings.pilot.place_class = PlaceClass::Open;

    let mut plane = World::new();
    plane.mavtype = ados_protocol::mavlink::ardupilotmega::MavType::MAV_TYPE_FIXED_WING;
    plane.run(100);
    assert_eq!(err(&plane.action("rc_receiver:absent")), Some("firmware"));

    w.airborne(copter::GUIDED);
    assert_eq!(err(&w.action("rc_receiver:absent")), Some("armed"));
}
