use ados_protocol::mavlink::ardupilotmega::MavLandedState;
use ados_protocol::pilot::Severity;

use super::*;
use crate::fc::rates::PidAxis;
use crate::fc::{Attitude, LocalPos, Stamped};

/// An ArduPilot quad (`FRAME_CLASS` 1) with this `FRAME_TYPE`.
fn quad(frame_type: f32) -> FcState {
    let mut fc = FcState {
        firmware: Some(crate::fc_profile::Firmware::Ardupilot),
        mav_type: 2,
        ..Default::default()
    };
    fc.params.insert("FRAME_CLASS".into(), 1.0);
    fc.params.insert("FRAME_TYPE".into(), frame_type);
    fc
}

/// One flight sample: everything fresh at `now`.
#[derive(Clone, Copy)]
struct Hover {
    i: [f32; 2],
    thr: f32,
    servo: [u16; 4],
    vd: f32,
    vn: f32,
    tilt_deg: f32,
    desired_dps: f32,
    landed: bool,
}

const STEADY: Hover = Hover {
    i: [0.0, 0.25],
    thr: 0.6,
    servo: [1800, 1700, 1800, 1700],
    vd: 0.0,
    vn: 0.0,
    tilt_deg: 1.0,
    desired_dps: 0.0,
    landed: false,
};

fn put(fc: &mut FcState, h: Hover, now: u64) {
    fc.armed = true;
    fc.landed_state = Some(if h.landed {
        MavLandedState::MAV_LANDED_STATE_ON_GROUND
    } else {
        MavLandedState::MAV_LANDED_STATE_IN_AIR
    });
    fc.local = stamp(
        LocalPos {
            vd: h.vd,
            vn: h.vn,
            ..Default::default()
        },
        now,
    );
    fc.attitude = stamp(
        Attitude {
            roll: h.tilt_deg.to_radians(),
            ..Default::default()
        },
        now,
    );
    let axis = |i| PidAxis {
        i,
        desired: h.desired_dps,
        ..Default::default()
    };
    fc.rates.roll = stamp(axis(h.i[0]), now);
    fc.rates.pitch = stamp(axis(h.i[1]), now);
    let s = h.servo;
    fc.rates.servo = stamp([s[0], s[1], s[2], s[3], 0, 0, 0, 0], now);
    fc.rates.throttle = stamp(h.thr, now);
}

fn stamp<T>(v: T, at_ms: u64) -> Option<Stamped<T>> {
    Some(Stamped { v, at_ms })
}

/// Fly `ms` from `from` at 25 Hz, `h(t)` the state at each sample.
fn fly(af: &mut Airframe, fc: &mut FcState, from: u64, ms: u64, h: impl Fn(u64) -> Hover) -> u64 {
    let mut now = from;
    while now < from + ms {
        now += 40;
        put(fc, h(now - from), now);
        af.tick(fc, now);
    }
    now
}

fn disarm(af: &mut Airframe, fc: &mut FcState, now: u64) {
    fc.armed = false;
    fc.landed_state = Some(MavLandedState::MAV_LANDED_STATE_ON_GROUND);
    af.tick(fc, now);
}

#[test]
fn headroom_at_the_measured_world_engine_points() {
    // ArduPilot defaults (PWM 1000-2000, spin 0.15-0.95, expo 0.65).
    let fc = FcState::default();
    for (pwm, headroom) in [(1740, 0.388), (1871, 0.157), (1898, 0.104)] {
        let h = 1.0 - thrust_demand(&fc, pwm);
        assert!((h - headroom).abs() < 0.005, "{pwm} µs: {h}");
    }
    // The FC's own range and curve when read.
    let mut fc = FcState::default();
    for (n, v) in [
        ("MOT_PWM_MIN", 1100.0),
        ("MOT_PWM_MAX", 1900.0),
        ("MOT_SPIN_MIN", 0.1),
        ("MOT_SPIN_MAX", 0.9),
        ("MOT_THST_EXPO", 0.0),
    ] {
        fc.params.insert(n.into(), v);
    }
    // a = 0.5, linear 0.5, no expo.
    assert!((thrust_demand(&fc, 1500) - 0.5).abs() < 1e-4);
    assert_eq!(thrust_demand(&fc, 1000), 0.0);
    assert_eq!(thrust_demand(&fc, 2000), 1.0);
}

#[test]
fn only_steady_hover_after_three_seconds_airborne_is_learned() {
    let not_steady: [(&str, Hover); 5] = [
        ("climbing", Hover { vd: -0.5, ..STEADY }),
        (
            "tilted",
            Hover {
                tilt_deg: 8.0,
                ..STEADY
            },
        ),
        (
            "rotating",
            Hover {
                desired_dps: 30.0,
                ..STEADY
            },
        ),
        (
            "on the ground",
            Hover {
                landed: true,
                ..STEADY
            },
        ),
        // Speeding up at 1 m/s².
        ("accelerating", STEADY),
    ];
    for (what, h) in not_steady {
        let mut af = Airframe::default();
        let mut fc = quad(1.0);
        fly(&mut af, &mut fc, 0, 10_000, |t| Hover {
            vn: if what == "accelerating" {
                t as f32 / 1000.0
            } else {
                h.vn
            },
            ..h
        });
        assert_eq!(af.trim(), None, "{what}");
        assert_eq!(af.record().busiest, None, "{what}");
    }
    // Steady, but only from 3 s airborne.
    let mut af = Airframe::default();
    let mut fc = quad(1.0);
    let now = fly(&mut af, &mut fc, 0, 2900, |_| STEADY);
    assert_eq!(af.trim(), None, "before 3 s airborne");
    fly(&mut af, &mut fc, now, 2000, |_| STEADY);
    let t = af.trim().expect("learned in steady hover");
    assert!(t.samples > 40, "{t:?}");
    assert!((t.i[1] - 0.25).abs() < 1e-4 && t.i[0].abs() < 1e-4, "{t:?}");
    assert!((t.hover_thr - 0.6).abs() < 1e-4, "{t:?}");
}

#[test]
fn the_averages_weigh_every_steady_sample() {
    let mut af = Airframe::default();
    let mut fc = quad(1.0);
    // Alternating 0.2 / 0.3 pitch I average to 0.25.
    fly(&mut af, &mut fc, 0, 20_000, |t| Hover {
        i: [0.0, if (t / 40) % 2 == 0 { 0.2 } else { 0.3 }],
        ..STEADY
    });
    let t = af.trim().unwrap();
    assert!((t.i[1] - 0.25).abs() < 0.01, "{t:?}");
    assert!(t.samples > 400);
}

#[test]
fn a_disarm_after_learning_and_a_lift_off_are_save_points() {
    let mut af = Airframe::default();
    let mut fc = quad(1.0);
    // Nothing to file before the FC identity is known.
    let now = fly(&mut af, &mut fc, 0, 6000, |_| STEADY);
    assert_eq!(af.save(), None);
    disarm(&mut af, &mut fc, now);
    assert_eq!(af.record().rev, 1);
    // The identity arrives: what was learned is filed under it, past the
    // stored revision.
    let stored = Record {
        rev: 4,
        ..Record::default()
    };
    af.load("fc-a", Some(stored), Some(10.0), None);
    let s = af.save().unwrap();
    assert_eq!(s.uid, "fc-a");
    assert_eq!(s.record.rev, 5);
    assert!(s.record.trim.is_some_and(|t| t.samples > 0));
    // A disarm with nothing learned is no save point.
    disarm(&mut af, &mut fc, now + 100);
    assert_eq!(af.record().rev, 5);
    af.note_liftoff(
        &LiftoffOutcome {
            i: Some([0.0, 0.26]),
            thrust: 0.66,
            max_motor: 0.8,
            saturated: false,
        },
        now,
    );
    assert_eq!(af.record().rev, 6);
}

#[test]
fn a_stored_record_loads_per_fc_and_round_trips() {
    let rec = Record {
        trim: Some(Trim {
            i: [-0.1, 0.25],
            hover_thr: 0.6,
            samples: 120,
        }),
        busiest: Some(0.7),
        busiest_samples: 120,
        saturated: false,
        stop_motor: 0.0,
        rev: 7,
    };
    let json = serde_json::to_string(&rec).unwrap();
    assert_eq!(serde_json::from_str::<Record>(&json).unwrap(), rec);
    let mut af = Airframe::default();
    af.load("fc-a", Some(rec.clone()), Some(10.0), None);
    assert_eq!(af.trim(), rec.trim);
    assert!((af.headroom().unwrap() - 0.3).abs() < 1e-6);
    // The same FC again (a new propeller size): the record stays.
    af.load("fc-a", None, Some(9.0), None);
    assert_eq!(af.record(), &rec);
    // An older setup state without the record reads as none.
    let old: crate::setup::step::WizardData =
        serde_json::from_str(r#"{"frame_class": 1, "prop_in": 10.0}"#).unwrap();
    assert_eq!(old.airframe, None);
}

/// A quad X with the 0.174 m half-spacing of the World Engine x500: 10"
/// props, tips 0.373 m out.
fn x500() -> (Airframe, FcState) {
    let mut af = Airframe::default();
    af.load("fc-a", None, Some(10.0), Some(0.373));
    (af, quad(1.0))
}

#[test]
fn the_centre_of_gravity_reads_in_mm_with_the_world_engine_signs() {
    let (af, fc) = x500();
    let w = |i, thr| af.imbalance_words(&fc, i, thr);
    // survey3d: +33 mm x.
    assert_eq!(
        w([0.0, 0.238], 0.61).as_deref(),
        Some("Nose-heavy. Off by about 35 mm. Move the battery back.")
    );
    // right40, tail40, nose50.
    assert_eq!(
        w([-0.277, 0.0], 0.59).as_deref(),
        Some("Right-heavy. Off by about 40 mm. Move the battery left.")
    );
    assert_eq!(
        w([0.0, -0.279], 0.59).as_deref(),
        Some("Tail-heavy. Off by about 40 mm. Move the battery forward.")
    );
    assert_eq!(
        w([0.0, 0.368], 0.62).as_deref(),
        Some("Nose-heavy. Off by about 50 mm. Move the battery back.")
    );
    // Both axes.
    assert_eq!(
        w([-0.2, 0.2], 0.6).as_deref(),
        Some("Nose-heavy and right-heavy. Off by about 40 mm. Move the battery back and left.")
    );
    // Balanced (x500-ados), and just under the 15 mm tolerance.
    assert_eq!(w([0.0, -0.002], 0.54), None);
    assert_eq!(w([0.0, 0.0966], 0.6), None);
    assert!(w([0.0, 0.106], 0.6).is_some());
}

#[test]
fn other_frames_read_a_share_of_the_control_margin() {
    let mut af = Airframe::default();
    af.load("fc-a", None, Some(10.0), None);
    // A quad H, or a quad X whose propeller size is unknown.
    for (fc, af) in [(quad(3.0), af.clone()), (quad(1.0), Airframe::default())] {
        assert_eq!(
            af.imbalance_words(&fc, [0.0, 0.3], 0.6).as_deref(),
            Some("Uneven. Using 30 % of the control margin to stay level.")
        );
        assert_eq!(af.imbalance_words(&fc, [0.0, 0.24], 0.6), None);
        assert_eq!(
            af.imbalance_words(&fc, [0.35, 0.3], 0.6).as_deref(),
            Some("Uneven. Using 35 % of the control margin to stay level.")
        );
    }
    // A quad + acts through half its motor radius.
    let mut af = Airframe::default();
    af.load("fc-a", None, Some(10.0), Some(0.373));
    let w = af.imbalance_words(&quad(0.0), [0.0, 0.25], 0.6);
    assert_eq!(
        w.as_deref(),
        Some("Nose-heavy. Off by about 25 mm. Move the battery back.")
    );
}

/// The setup-strings text `key`.
fn spec(key: &str) -> String {
    let spec: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../web-shared/src/cockpit-spec/setup-strings.json"
    ))
    .unwrap();
    spec["strings"][key].as_str().unwrap().to_owned()
}

#[test]
fn the_balance_row_learns_then_passes_or_warns_in_the_spec_words() {
    let (mut af, mut fc) = x500();
    let r = af.readiness(&fc).expect("the row while learning");
    assert_eq!(
        (r.id.as_str(), r.ok, r.msg.as_str()),
        (
            "airframe_balance",
            true,
            "Balance: learning. Learned after the first steady hover."
        )
    );
    assert_eq!(r.msg, spec("readiness.airframe_balance.learning"));
    // Only on an ArduPilot multirotor.
    let mut px4 = fc.clone();
    px4.firmware = Some(crate::fc_profile::Firmware::Px4);
    assert_eq!(af.readiness(&px4), None);
    // Balanced and learned.
    let now = fly(&mut af, &mut fc, 0, 6000, |_| Hover {
        i: [0.0, 0.01],
        ..STEADY
    });
    let r = af.readiness(&fc).unwrap();
    assert_eq!((r.ok, r.msg.as_str()), (true, "Balance: OK."));
    assert_eq!(r.msg, spec("readiness.airframe_balance.ok"));
    // Nose-heavy.
    let mut af = x500().0;
    fly(&mut af, &mut fc, now, 6000, |_| STEADY);
    let r = af.readiness(&fc).expect("a warning");
    assert_eq!(
        (r.id.as_str(), r.ok, r.severity),
        ("airframe_balance", false, Severity::Warn)
    );
    assert_eq!(
        r.msg,
        "Balance: nose-heavy. Off by about 35 mm. Move the battery back."
    );
    let want = spec("readiness.airframe_balance.heavy")
        .replace("{sides}", "nose-heavy")
        .replace("{mm}", "35")
        .replace("{moves}", "back");
    assert_eq!(r.msg, want);
    // Another frame type.
    let mut fc = quad(3.0);
    let mut af = x500().0;
    fly(&mut af, &mut fc, 0, 6000, |_| Hover {
        i: [0.0, 0.3],
        ..STEADY
    });
    let r = af.readiness(&fc).unwrap();
    assert_eq!((r.ok, r.severity), (false, Severity::Warn));
    assert_eq!(
        r.msg,
        spec("readiness.airframe_balance.uneven").replace("{pct}", "30")
    );
}

#[test]
fn too_little_motor_headroom_in_hover_blocks() {
    let (mut af, mut fc) = x500();
    // 1871 µs (40 mm offsets): 15.7 % left, a warning only.
    let now = fly(&mut af, &mut fc, 0, 6000, |_| Hover {
        servo: [1871, 1600, 1871, 1600],
        ..STEADY
    });
    let r = af.readiness(&fc).unwrap();
    assert_eq!(r.severity, Severity::Warn);
    // 1910 µs: 8 % left.
    let mut af = x500().0;
    fly(&mut af, &mut fc, now, 6000, |_| Hover {
        servo: [1910, 1600, 1910, 1600],
        ..STEADY
    });
    let r = af.readiness(&fc).unwrap();
    assert_eq!((r.ok, r.severity), (false, Severity::Block));
    assert_eq!(
        r.msg,
        "Can't take off: no climb margin. Motors at 92 % in hover. Lighten the drone or rebalance it."
    );
    assert_eq!(
        r.msg,
        spec("readiness.airframe_balance.headroom").replace("{pct}", "92")
    );
}

#[test]
fn a_lift_off_stopped_for_a_saturated_motor_blocks_until_reset() {
    let (mut af, fc) = x500();
    af.note_liftoff(
        &LiftoffOutcome {
            i: None,
            thrust: 0.69,
            max_motor: 0.95,
            saturated: true,
        },
        0,
    );
    let r = af.readiness(&fc).unwrap();
    assert_eq!(
        (r.id.as_str(), r.ok, r.severity),
        ("airframe_balance", false, Severity::Block)
    );
    assert_eq!(
        r.msg,
        "Can't take off: no climb margin. Motors at 95 % at take-off. Lighten the drone or rebalance it."
    );
    assert_eq!(
        r.msg,
        spec("readiness.airframe_balance.saturated").replace("{pct}", "95")
    );
    // "I rebalanced it": forgotten, and a save point.
    let rev = af.record().rev;
    af.reset();
    assert_eq!(
        af.readiness(&fc).map(|r| r.ok),
        Some(true),
        "learning again"
    );
    assert_eq!(af.trim(), None);
    assert_eq!(af.record().rev, rev + 1);
    assert_eq!(
        af.save().unwrap().record,
        Record {
            rev: rev + 1,
            ..Record::default()
        }
    );
}

#[test]
fn a_lift_off_seeds_the_trim_and_moves_a_learned_one() {
    let (mut af, mut fc) = x500();
    let lifted = |i| LiftoffOutcome {
        i: Some(i),
        thrust: 0.66,
        max_motor: 0.8,
        saturated: false,
    };
    // First flight: the probe's trim.
    af.note_liftoff(&lifted([0.0, 0.24]), 0);
    assert_eq!(
        af.trim(),
        Some(Trim {
            i: [0.0, 0.24],
            hover_thr: 0.66,
            samples: 0
        })
    );
    // Learned in hover, then a lift-off close to it nudges it.
    let now = fly(&mut af, &mut fc, 0, 6000, |_| STEADY);
    let learned = af.trim().unwrap();
    af.note_liftoff(&lifted([0.0, 0.30]), now);
    let t = af.trim().unwrap();
    assert!((t.i[1] - 0.26).abs() < 1e-4, "{t:?}");
    assert_eq!(
        (t.samples, t.hover_thr),
        (learned.samples, learned.hover_thr)
    );
    // One far from it: the airframe changed, the learning restarts there.
    af.note_liftoff(&lifted([0.0, 0.05]), now);
    assert_eq!(af.trim().map(|t| (t.i, t.samples)), Some(([0.0, 0.05], 0)));
    // A clean lift-off clears an earlier saturated stop.
    af.note_liftoff(
        &LiftoffOutcome {
            saturated: true,
            ..lifted([0.0, 0.05])
        },
        now,
    );
    assert!(af.record().saturated);
    af.note_liftoff(&lifted([0.0, 0.05]), now);
    assert!(!af.record().saturated);
}
