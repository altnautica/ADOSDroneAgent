//! The lift-off state machine against the test airframe.

use super::sim::Airframe;
use super::*;

/// A lift-off flown against `af` for at most `ms`: the peak tilt from rest
/// on the ground and up to 1.5 s after lift-off, the lift-off and stop.
struct Run {
    lo: Liftoff,
    af: Airframe,
    now: u64,
    peak_deg: f32,
    ground_peak_deg: f32,
    lifted_ms: Option<u64>,
    /// The airframe's height when the lift-off first said it had lifted.
    h_at_lift: Option<f32>,
    /// The FC's height estimate creeps up this fast while the airframe sits
    /// on its skids (m/s), and how far it has crept.
    ekf_creep_mps: f32,
    ekf_up: f32,
    /// The rangefinder's noise, ± m.
    agl_noise_m: f32,
}

impl Run {
    fn new(af: Airframe, trim: Option<Trim>) -> Self {
        let mut af = af;
        af.armed = true;
        Self {
            lo: Liftoff::new(trim, af.hover, 3.0, 0),
            af,
            now: 0,
            peak_deg: 0.0,
            ground_peak_deg: 0.0,
            lifted_ms: None,
            h_at_lift: None,
            ekf_creep_mps: 0.0,
            ekf_up: 0.0,
            agl_noise_m: 0.0,
        }
    }

    fn inputs(&self) -> Inputs {
        let m = self.af.motors();
        let frac = |v: u16| (f32::from(v) - 1000.0) / 1000.0;
        let max = *m.iter().max().unwrap();
        let min = *m.iter().min().unwrap();
        Inputs {
            now_ms: self.now,
            att: Some(Attitude {
                roll: self.af.att[0],
                pitch: self.af.att[1],
                yaw: self.af.att[2],
                yaw_rate: 0.0,
            }),
            local: Some(LocalPos {
                d: -(self.af.h + self.ekf_up),
                vd: -self.af.vz
                    - if self.af.airborne() {
                        0.0
                    } else {
                        self.ekf_creep_mps
                    },
                ..Default::default()
            }),
            i: Some(self.af.i),
            motors: Some(Motors {
                max: frac(max),
                min: frac(min),
                spread_us: max - min,
            }),
            agl: Some(0.1 + self.af.h + self.agl_noise_m * noise(self.now)),
        }
    }

    fn run(&mut self, ms: u64) {
        let end = self.now + ms;
        while self.now < end {
            self.now += 20;
            let inp = self.inputs();
            let cmd = self.lo.step(&inp);
            self.fly(cmd);
        }
    }

    /// Send `cmd` to the airframe and advance it one tick.
    fn fly(&mut self, cmd: Option<Cmd>) {
        match cmd {
            Some(Cmd::Attitude {
                roll,
                pitch,
                yaw,
                thrust,
            }) => self.af.target = Some(([roll, pitch, yaw], thrust)),
            Some(Cmd::Position { pos, .. }) => self.af.goal_h = Some(-pos[2]),
            None => {
                if let Some(t) = self.af.target.as_mut() {
                    t.1 = 0.0;
                }
                // Stopped on the ground: the pilot disarms, and the FC's
                // integrators stop winding against the last target.
                if self.lo.stopped().is_some() && !self.lo.lifted_ever() {
                    self.af.armed = false;
                }
            }
        }
        for _ in 0..4 {
            self.af.step(0.005);
        }
        if !self.af.airborne() {
            self.ekf_up += 0.02 * self.ekf_creep_mps;
        }
        if self.lifted_ms.is_none() && self.lo.lifted_ever() {
            self.lifted_ms = Some(self.now);
            self.h_at_lift = Some(self.af.h);
        }
        let tilt = self.af.att[0].abs().max(self.af.att[1].abs()).to_degrees();
        if self.lifted_ms.is_none() {
            self.ground_peak_deg = self.ground_peak_deg.max(tilt);
        }
        if self.lifted_ms.is_none_or(|t| self.now <= t + 1500) {
            self.peak_deg = self.peak_deg.max(tilt);
        }
    }
}

/// A repeatable rangefinder noise in −1..1, a new value every tick.
fn noise(now_ms: u64) -> f32 {
    let x = (now_ms / 20).wrapping_mul(2_654_435_761) % 1000;
    x as f32 / 500.0 - 1.0
}

fn learned(i: [f32; 2], hover: f32) -> Option<Trim> {
    Some(Trim {
        i,
        hover_thr: hover,
        samples: 50,
    })
}

#[test]
fn balanced_lifts_off_level_and_hands_over() {
    let mut r = Run::new(Airframe::balanced(), None);
    r.run(12_000);
    assert!(r.lo.handed_over(), "{:?}", r.lo.view());
    assert!(r.peak_deg < 1.0, "peak {}", r.peak_deg);
    assert_eq!(r.lo.edges, 0, "nothing unloads on a balanced airframe");
    assert!(r.af.h > 0.25);
}

#[test]
fn learned_trim_loads_on_the_ground_then_lifts_level() {
    let af = Airframe::new(0.61, [0.0, 0.25]);
    let mut r = Run::new(af, learned([0.0, 0.25], 0.61));
    r.run(15_000);
    assert!(r.lo.handed_over(), "{:?}", r.lo.view());
    assert_eq!(r.lo.edges, 0, "no side unloads with the right trim");
    assert!(r.peak_deg < 2.0, "peak {}", r.peak_deg);
}

#[test]
fn first_flight_finds_the_trim_on_the_skids_without_pivoting() {
    // survey3d, right40, tail40, a corner offset.
    for trim in [[0.0, 0.25], [-0.29, 0.0], [0.0, -0.28], [0.12, 0.15]] {
        let af = Airframe::new(0.6, trim);
        let mut r = Run::new(af, None);
        r.run(40_000);
        assert!(r.lo.handed_over(), "{trim:?}: {:?}", r.lo.view());
        assert!(r.lo.edges > 0, "{trim:?}");
        assert!(
            r.ground_peak_deg <= HARD_DEG,
            "{trim:?}: pivoted {} on the skids",
            r.ground_peak_deg
        );
        assert!(r.peak_deg < 5.0, "{trim:?}: peak {}", r.peak_deg);
        let o = r.lo.take_outcome().expect("the lift-off's outcome");
        let i = o.i.expect("the integrators at lift-off");
        for ax in 0..2 {
            assert!((i[ax] - trim[ax]).abs() < 0.08, "{trim:?}: {i:?}");
        }
        assert!(!o.saturated, "{trim:?}: {o:?}");
    }
}

#[test]
fn a_height_estimate_creeping_up_on_the_skids_is_not_a_lift_off() {
    // On the bench (right40, tail40, nose35) the FC's height crept up while
    // the airframe rocked on its skids finding its trim, so the lift-off
    // took it as lifted, levelled the attitude target and handed a
    // grounded, unbalanced airframe to the position controller, which
    // pivoted it 6-15°. The rangefinder (noisy) read the skids still down.
    for trim in [[0.0, 0.0], [0.0, 0.25], [-0.29, 0.0], [0.0, -0.28]] {
        let mut r = Run::new(Airframe::new(0.6, trim), None);
        r.ekf_creep_mps = 0.05;
        r.agl_noise_m = 0.07;
        r.run(40_000);
        assert!(r.lo.handed_over(), "{trim:?}: {:?}", r.lo.view());
        let h = r.h_at_lift.expect("a lift-off");
        assert!(h > 0.0, "{trim:?}: took the ground for a lift-off");
        // …and seen while still close to it, not a median's lag later.
        assert!(h < 0.15, "{trim:?}: lift-off seen at {h:.3} m");
        assert!(r.peak_deg < 5.0, "{trim:?}: peak {}", r.peak_deg);
    }
}

#[test]
fn wrong_learned_trim_is_corrected_on_the_skids() {
    // The battery moved since the trim was learned.
    let af = Airframe::new(0.6, [0.0, -0.2]);
    let mut r = Run::new(af, learned([0.0, 0.25], 0.6));
    r.run(40_000);
    assert!(r.lo.handed_over(), "{:?}", r.lo.view());
    assert!(r.lo.edges > 0);
    assert!(
        r.ground_peak_deg <= HARD_DEG,
        "pivoted {}",
        r.ground_peak_deg
    );
    assert!(r.peak_deg < 5.0, "peak {}", r.peak_deg);
}

#[test]
fn a_motor_at_the_top_of_its_range_stops_on_the_ground() {
    // nose50: the trim it needs leaves no motor headroom at lift-off.
    let af = Airframe::new(0.62, [0.0, 0.42]);
    let mut r = Run::new(af, learned([0.0, 0.42], 0.62));
    r.run(30_000);
    assert_eq!(r.lo.stopped(), Some(Stop::Saturated), "{:?}", r.lo.view());
    assert!(!r.lo.lifted_ever());
    let o = r.lo.take_outcome().expect("saturation outcome");
    assert!(o.saturated && o.max_motor > SATURATION);
    assert_eq!(r.lo.view().phase, "stopped");
}

/// The Balance check is what refuses nose50's next launch: filed with the
/// saturated stop it blocks, and a check stubbed healthy (the mutation)
/// would let the same airframe launch again. A gate that still refused under
/// the stub would be refusing for some other reason.
#[test]
fn a_forced_healthy_balance_check_lets_nose50_launch() {
    let af = Airframe::new(0.62, [0.0, 0.42]);
    let mut r = Run::new(af, learned([0.0, 0.42], 0.62));
    r.run(30_000);
    let o = r.lo.take_outcome().expect("saturation outcome");
    let mut health = crate::airframe::Airframe::default();
    health.note_liftoff(&o, r.now);
    let mut fc = crate::fc::FcState {
        firmware: Some(crate::fc_profile::Firmware::Ardupilot),
        mav_type: 2,
        ..Default::default()
    };
    fc.params.insert("FRAME_CLASS".into(), 1.0);
    fc.params.insert("FRAME_TYPE".into(), 1.0);
    let row = health.readiness(&fc).expect("the airframe_balance row");
    assert_eq!(
        (row.ok, row.severity),
        (false, ados_protocol::pilot::Severity::Block),
        "the gate refuses nose50: {row:?}"
    );
    health.force_healthy = true;
    let forced = health.readiness(&fc).expect("the airframe_balance row");
    assert!(
        forced.ok && forced.severity != ados_protocol::pilot::Severity::Block,
        "stubbed healthy, nose50 would launch: {forced:?}"
    );
}

/// Inputs for a hand-built tick: level, at `local`, motors at `motor_max`.
fn tick(now_ms: u64, local: LocalPos, motor_max: f32) -> Inputs {
    Inputs {
        now_ms,
        att: Some(Attitude::default()),
        local: Some(local),
        i: Some([0.0; 2]),
        motors: Some(Motors {
            max: motor_max,
            min: motor_max - 0.01,
            spread_us: 10,
        }),
        agl: None,
    }
}

/// `tick`, tilted `deg` nose-down from rest.
fn tipped(now_ms: u64, deg: f32) -> Inputs {
    Inputs {
        att: Some(Attitude {
            pitch: (-deg).to_radians(),
            ..Default::default()
        }),
        ..tick(now_ms, LocalPos::default(), 0.7)
    }
}

/// A lift-off past its spool, ramping on the ground at `now_ms`.
fn ramping(now_ms: u64) -> Liftoff {
    let mut lo = Liftoff::new(None, 0.6, 1.5, 0);
    lo.start = Some(LocalPos::default());
    lo.rest = Some((0.0, 0.0));
    lo.phase = Phase::Ramp;
    lo.thrust = 0.5;
    lo.last_ms = now_ms;
    lo
}

#[test]
fn a_motor_at_the_top_for_a_moment_is_not_a_stop() {
    // A trim correction can peak one motor briefly; only a motor held at
    // the top stops the launch.
    let mut lo = ramping(1000);
    let ground = LocalPos::default();
    let mut now = 1000;
    for _ in 0..5 {
        now += 20;
        assert!(lo.step(&tick(now, ground, 0.97)).is_some());
    }
    now += 20;
    lo.step(&tick(now, ground, 0.8));
    assert_eq!(lo.stopped(), None, "{:?}", lo.view());
    for _ in 0..10 {
        now += 20;
        lo.step(&tick(now, ground, 0.97));
    }
    assert_eq!(lo.stopped(), Some(Stop::Saturated));
}

#[test]
fn a_motor_short_of_the_top_on_the_skids_is_not_a_stop() {
    // A 40 mm offset peaks its busiest motor near 0.90 on the skids.
    let mut lo = ramping(1000);
    let mut now = 1000;
    for _ in 0..50 {
        now += 20;
        lo.step(&tick(now, LocalPos::default(), 0.905));
    }
    assert_eq!(lo.stopped(), None, "{:?}", lo.view());
}

#[test]
fn an_unloading_side_holds_the_thrust_and_leans_toward_it() {
    let mut lo = ramping(1000);
    let Some(Cmd::Attitude { pitch, thrust, .. }) = lo.step(&tipped(1020, 0.7)) else {
        panic!("an attitude target: {:?}", lo.phase);
    };
    // Nose down: the target leans nose-up, the thrust holds.
    assert!((pitch - EDGE_LEAN_DEG.to_radians()).abs() < 1e-4, "{pitch}");
    assert!((thrust - 0.5).abs() < 1e-4, "{thrust}");
    assert_eq!(lo.view().text, "Checking balance");
    // Level again: the lean stays until the other side unloads, the thrust
    // rises again.
    let Some(Cmd::Attitude { pitch, thrust, .. }) = lo.step(&tipped(1040, 0.1)) else {
        panic!();
    };
    assert!(pitch > 0.0 && thrust > 0.5, "{pitch} {thrust}");
    lo.step(&tipped(1060, -0.7));
    let Some(Cmd::Attitude { pitch, .. }) = lo.step(&tipped(1080, -0.7)) else {
        panic!();
    };
    assert!(pitch < 0.0, "turned round: {pitch}");
    assert_eq!(lo.edges, 2);
}

#[test]
fn a_tilt_past_the_back_off_steps_the_thrust_back_once_and_a_bigger_one_cuts_it() {
    let mut lo = ramping(1000);
    lo.step(&tipped(1020, BACK_DEG + 0.2));
    assert!(
        (lo.thrust - (0.5 - BACK_STEP * 0.6)).abs() < 1e-4,
        "{}",
        lo.thrust
    );
    lo.step(&tipped(1040, BACK_DEG + 0.4));
    assert!(
        (lo.thrust - (0.5 - BACK_STEP * 0.6)).abs() < 1e-4,
        "once per excursion: {}",
        lo.thrust
    );
    lo.step(&tipped(1060, HARD_DEG + 0.2));
    assert!((lo.thrust - HARD_FRAC * 0.6).abs() < 1e-4, "{}", lo.thrust);
    assert_eq!(lo.stopped(), None);
}

#[test]
fn the_bounce_after_a_cut_does_not_turn_the_lean_round() {
    // A nose-heavy airframe pivots, the cut drops it back onto its skids and
    // it overshoots nose-up. Turning the lean round there unwound the trim
    // the integrator had wound, and the ramp cycled on the skids.
    let pitch_of = |c: Option<Cmd>| match c {
        Some(Cmd::Attitude { pitch, .. }) => pitch,
        other => panic!("an attitude target: {other:?}"),
    };
    let mut lo = ramping(1000);
    assert!(pitch_of(lo.step(&tipped(1020, 0.7))) > 0.0);
    lo.step(&tipped(1040, HARD_DEG + 0.5));
    assert!((lo.thrust - HARD_FRAC * 0.6).abs() < 1e-4, "{}", lo.thrust);
    // The bounce: nose-up past the edge, just after the cut.
    assert!(
        pitch_of(lo.step(&tipped(1060, -0.8))) > 0.0,
        "the lean held through the bounce"
    );
    assert_eq!(lo.edges, 1);
    // Settled, the ramp climbs back; once past the quiet window and at
    // ramp thrust, the other side unloading turns the lean round.
    let mut now = 1060;
    while now < 1060 + FLIP_QUIET_MS + 200 {
        now += 20;
        lo.step(&tipped(now, 0.0));
    }
    assert!(lo.thrust > K_FAST * 0.6, "{}", lo.thrust);
    now += 20;
    assert!(pitch_of(lo.step(&tipped(now, -0.7))) < 0.0, "turned round");
    assert_eq!(lo.edges, 2);
    assert_eq!(lo.stopped(), None);
}

#[test]
fn a_stop_reads_the_same_on_the_launch_and_in_the_spec() {
    // A stopped launch shows "Take-off stopped" and the stop's text; a
    // refusal or a setup step shows the spec string for its code. They are
    // the same words.
    let spec: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../web-shared/src/cockpit-spec/setup-strings.json"
    ))
    .unwrap();
    for stop in [
        Stop::Tipping,
        Stop::Saturated,
        Stop::NoTrim,
        Stop::NoRateData,
        Stop::Spool,
        Stop::FcLink,
        Stop::Link,
        Stop::Operator,
        Stop::Timeout,
    ] {
        let shown = format!("{STOPPED_TEXT}. {}", stop.text());
        for key in ["refusal", "setup.blocker"] {
            let key = format!("{key}.{}", stop.code());
            assert_eq!(
                spec["strings"][&key].as_str(),
                Some(shown.as_str()),
                "{key}"
            );
        }
    }
    let params = "liftoff_fc_params";
    assert_eq!(
        spec["strings"][format!("refusal.{params}")].as_str(),
        error_text(params)
    );
}

#[test]
fn a_tilt_past_the_limit_stops_on_the_ground() {
    let mut lo = ramping(1000);
    assert!(lo.step(&tipped(1020, ABORT_DEG - 0.2)).is_some());
    assert!(lo.step(&tipped(1040, ABORT_DEG + 0.2)).is_none());
    assert_eq!(lo.stopped(), Some(Stop::Tipping));
    assert_eq!(lo.view().text, STOPPED_TEXT);
    assert_eq!(lo.view().detail, Stop::Tipping.text());
}

/// One row of `fixtures/nose_heavy_pivot.csv`.
struct Row {
    t_ms: u64,
    roll_deg: f32,
    pitch_deg: f32,
    pitch_i: f32,
    max_us: f32,
    min_us: f32,
}

fn pivot_rows() -> Vec<Row> {
    include_str!("fixtures/nose_heavy_pivot.csv")
        .lines()
        .filter(|l| !l.starts_with('#') && !l.starts_with("t_ms"))
        .map(|l| {
            let v: Vec<f32> = l.split(',').map(|x| x.parse().unwrap()).collect();
            Row {
                t_ms: v[0] as u64,
                roll_deg: v[1],
                pitch_deg: v[2],
                pitch_i: v[3],
                max_us: v[4],
                min_us: v[5],
            }
        })
        .collect()
}

#[test]
fn a_nose_heavy_pivot_stops_well_before_the_tilt_it_rolled_over_from() {
    // Replayed from a 50 mm nose-heavy quad that pivoted onto its front
    // skids; stopped at 10.9°, it still rolled over.
    let rows = pivot_rows();
    let mut lo = Liftoff::new(None, 0.62, 1.5, 0);
    let mut rest = None;
    let mut stop = None;
    for r in &rows {
        let inp = Inputs {
            now_ms: r.t_ms,
            att: Some(Attitude {
                roll: r.roll_deg.to_radians(),
                pitch: r.pitch_deg.to_radians(),
                ..Default::default()
            }),
            local: Some(LocalPos::default()),
            i: Some([0.0, r.pitch_i]),
            motors: Some(Motors {
                max: (r.max_us - 1000.0) / 1000.0,
                min: (r.min_us - 1000.0) / 1000.0,
                spread_us: (r.max_us - r.min_us) as u16,
            }),
            agl: None,
        };
        if lo.step(&inp).is_none() {
            stop = Some((r.t_ms, (r.pitch_deg - rest.unwrap_or(r.pitch_deg)).abs()));
            break;
        }
        rest = lo.rest.map(|(_, p)| p.to_degrees());
    }
    let (t_stop, tilt) = stop.expect("a stop on the ground");
    assert_eq!(lo.stopped(), Some(Stop::Tipping), "{:?}", lo.view());
    // At most one sample (40 ms) past the limit.
    assert!(tilt <= ABORT_DEG + 1.5, "stopped at {tilt}°");
    // Well before the airframe reached the tilt it rolled over from.
    let rest = rest.expect("the resting attitude");
    let t_rolled = rows
        .iter()
        .find(|r| (r.pitch_deg - rest).abs() > 10.0)
        .map(|r| r.t_ms)
        .expect("the recorded pivot passes 10°");
    assert!(
        t_rolled >= t_stop + 80,
        "stop {t_stop} ms, 10° at {t_rolled} ms"
    );
}

#[test]
fn leaning_without_rate_data_stops_instead_of_guessing() {
    let af = Airframe::new(0.6, [0.0, 0.25]);
    let mut r = Run::new(af, None);
    for _ in 0..2000 {
        r.now += 20;
        let mut inp = r.inputs();
        inp.i = None;
        let cmd = r.lo.step(&inp);
        r.fly(cmd);
        if r.lo.stopped().is_some() {
            break;
        }
    }
    assert_eq!(r.lo.stopped(), Some(Stop::NoRateData));
    assert!(!r.lo.lifted_ever());
    assert!(
        r.ground_peak_deg <= HARD_DEG,
        "pivoted {}",
        r.ground_peak_deg
    );
}

#[test]
fn a_trim_beyond_the_rate_loop_stops_on_the_skids() {
    let af = Airframe::new(0.6, [0.0, 0.7]);
    let mut r = Run::new(af, None);
    r.run(40_000);
    assert_eq!(r.lo.stopped(), Some(Stop::NoTrim), "{:?}", r.lo.view());
    assert!(!r.lo.lifted_ever());
    assert!(
        r.ground_peak_deg < ABORT_DEG,
        "tilted {}",
        r.ground_peak_deg
    );
}

#[test]
fn spool_waits_for_steady_motors_and_times_out_without_them() {
    let af = Airframe::balanced();
    let mut r = Run::new(af, None);
    let mut inp = r.inputs();
    inp.motors = Some(Motors {
        max: 0.0,
        min: 0.0,
        spread_us: 0,
    });
    for t in (20..=9000).step_by(20) {
        inp.now_ms = t;
        r.lo.step(&inp);
    }
    assert_eq!(r.lo.stopped(), Some(Stop::Spool));
}

#[test]
fn an_operator_stop_on_the_ground_stops_and_one_in_the_air_does_not() {
    for at in [1000, 2500, 4500] {
        let mut r = Run::new(Airframe::new(0.6, [0.0, 0.25]), None);
        r.run(at);
        assert!(r.lo.on_ground());
        r.lo.stop(Stop::Operator);
        assert_eq!(r.lo.stopped(), Some(Stop::Operator));
        assert!(r.lo.step(&r.inputs()).is_none());
    }
    let mut r = Run::new(Airframe::balanced(), None);
    r.run(12_000);
    assert!(r.lo.handed_over());
    r.lo.stop(Stop::Operator);
    assert_eq!(r.lo.stopped(), None);
}

#[test]
fn the_fc_going_quiet_on_the_ground_stops() {
    let mut r = Run::new(Airframe::balanced(), None);
    r.run(1000);
    let mut inp = r.inputs();
    inp.att = None;
    assert!(r.lo.step(&inp).is_none());
    assert_eq!(r.lo.stopped(), Some(Stop::FcLink));
}

#[test]
fn view_names_the_phases_in_operator_words() {
    let mut r = Run::new(Airframe::new(0.61, [0.0, 0.25]), learned([0.0, 0.25], 0.61));
    let v = r.lo.view();
    assert_eq!(
        (v.phase.as_str(), v.text.as_str()),
        ("spool", "Spinning up")
    );
    r.run(1500);
    let v = r.lo.view();
    assert_eq!(
        (v.phase.as_str(), v.text.as_str()),
        ("level", "Setting trim")
    );
    assert_eq!(v.detail, "Loading this airframe's learned balance");
    assert_eq!(v.trim, "learned");
    r.run(15_000);
    let v = r.lo.view();
    assert_eq!(
        (v.text.as_str(), v.detail.as_str()),
        ("Airborne", "Climbing to 3 m")
    );
    let p = Run::new(Airframe::balanced(), None);
    assert_eq!(p.lo.view().trim, "probe");
}

/// A lift-off off its skids and climbing level at `now_ms`.
fn climbing(now_ms: u64) -> Liftoff {
    let mut lo = ramping(now_ms);
    lo.lifted = true;
    lo.thrust = 0.65;
    lo.phase = Phase::Climb { since: now_ms };
    lo
}

#[test]
fn the_hand_over_starts_from_the_airframes_own_motion() {
    let mut lo = climbing(1000);
    let here = LocalPos {
        n: 0.02,
        e: -0.01,
        d: -0.3,
        vn: 0.2,
        ve: 0.0,
        vd: -0.3,
    };
    let Some(Cmd::Position { pos, vel, .. }) = lo.step(&tick(1020, here, 0.85)) else {
        panic!("a position target at the hand-over: {:?}", lo.phase);
    };
    // No step: the target is where the airframe is, moving as it moves.
    assert!(
        (pos[0] - 0.02).abs() < 0.01 && (pos[1] + 0.01).abs() < 0.01,
        "{pos:?}"
    );
    assert!(
        (vel[0] - 0.2).abs() < 0.02 && vel[1].abs() < 1e-3,
        "{vel:?}"
    );
    assert!((vel[2] + 0.3).abs() < 0.02, "climb {:?}", vel[2]);
    // A second later the drift has eased to rest within a few cm, and the
    // climb has sped up toward its rate.
    let mut now = 1020;
    let mut last = (pos, vel);
    for _ in 0..50 {
        now += 20;
        if let Some(Cmd::Position { pos, vel, .. }) = lo.step(&tick(now, here, 0.85)) {
            last = (pos, vel);
        }
    }
    let (pos, vel) = last;
    assert!(vel[0].abs() < 1e-3, "{vel:?}");
    assert!(
        pos[0] - 0.02 < 0.2 * 0.2 / (2.0 * DRIFT_DECEL_MPS2) + 0.01,
        "{pos:?}"
    );
    assert!(vel[2] < -0.5 + 0.01, "{vel:?}");
}

#[test]
fn a_motor_near_the_top_slows_the_climb() {
    let mut lo = climbing(1000);
    let here = LocalPos {
        d: -0.3,
        vd: -0.3,
        ..Default::default()
    };
    lo.step(&tick(1020, here, 0.85));
    let mut now = 1020;
    let mut climb = 0.0;
    for _ in 0..50 {
        now += 20;
        if let Some(Cmd::Position { vel, .. }) = lo.step(&tick(now, here, 0.95)) {
            climb = -vel[2];
        }
    }
    assert!((climb - CLIMB_MIN_MPS).abs() < 0.02, "climb {climb}");
}
