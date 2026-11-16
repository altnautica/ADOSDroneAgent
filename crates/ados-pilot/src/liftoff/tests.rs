//! The lift-off state machine against the test airframe.

use super::sim::Airframe;
use super::*;

/// A lift-off flown against `af` for at most `ms`: the peak tilt from rest
/// up to 1.5 s after lift-off, the lift-off and stop.
struct Run {
    lo: Liftoff,
    af: Airframe,
    now: u64,
    peak_deg: f32,
    lifted_ms: Option<u64>,
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
            lifted_ms: None,
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
                d: -self.af.h,
                vd: -self.af.vz,
                ..Default::default()
            }),
            i: Some(self.af.i),
            motors: Some(Motors {
                max: frac(max),
                min: frac(min),
                spread_us: max - min,
            }),
            agl: Some(0.1 + self.af.h),
        }
    }

    fn run(&mut self, ms: u64) {
        let end = self.now + ms;
        while self.now < end {
            self.now += 20;
            let inp = self.inputs();
            match self.lo.step(&inp) {
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
                }
            }
            for _ in 0..4 {
                self.af.step(0.005);
            }
            if self.lifted_ms.is_none() && self.lo.lifted_ever() {
                self.lifted_ms = Some(self.now);
            }
            if self.lifted_ms.is_none_or(|t| self.now <= t + 1500) {
                let tilt = self.af.att[0].abs().max(self.af.att[1].abs()).to_degrees();
                self.peak_deg = self.peak_deg.max(tilt);
            }
        }
    }
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
    assert!(r.af.h > 0.25);
}

#[test]
fn learned_trim_loads_on_the_ground_then_lifts_level() {
    let af = Airframe::new(0.61, [0.0, 0.25]);
    let mut r = Run::new(af, learned([0.0, 0.25], 0.61));
    r.run(15_000);
    assert!(r.lo.handed_over(), "{:?}", r.lo.view());
    assert_eq!(r.lo.probes, 0, "no probe with the right trim");
    assert!(r.peak_deg < 5.0, "peak {}", r.peak_deg);
}

#[test]
fn first_flight_probes_to_the_trim_then_lifts_level() {
    for trim in [[0.0, 0.25], [-0.29, 0.0], [0.0, -0.28], [0.12, 0.15]] {
        let af = Airframe::new(0.6, trim);
        let mut r = Run::new(af, None);
        r.run(40_000);
        assert!(r.lo.handed_over(), "{trim:?}: {:?}", r.lo.view());
        assert!(r.lo.probes > 0, "{trim:?}");
        assert!(r.peak_deg < 5.0, "{trim:?}: peak {}", r.peak_deg);
        let o = r.lo.take_outcome().expect("the lift-off's outcome");
        assert!(o.i.is_some() && !o.saturated, "{trim:?}: {o:?}");
    }
}

#[test]
fn wrong_learned_trim_is_corrected_by_the_probe() {
    // The battery moved since the trim was learned.
    let af = Airframe::new(0.6, [0.0, -0.2]);
    let mut r = Run::new(af, learned([0.0, 0.25], 0.6));
    r.run(40_000);
    assert!(r.lo.handed_over(), "{:?}", r.lo.view());
    assert!(r.lo.probes > 0);
    assert!(r.peak_deg < 5.0, "peak {}", r.peak_deg);
}

#[test]
fn a_motor_at_the_top_of_its_range_stops_on_the_ground() {
    // nose50: the trim it needs leaves no motor headroom at lift-off.
    let af = Airframe::new(0.62, [0.0, 0.37]);
    let mut r = Run::new(af, learned([0.0, 0.37], 0.62));
    r.run(30_000);
    assert_eq!(r.lo.stopped(), Some(Stop::Saturated), "{:?}", r.lo.view());
    assert!(!r.lo.lifted_ever());
    let o = r.lo.take_outcome().expect("saturation outcome");
    assert!(o.saturated && o.max_motor > SATURATION);
    assert_eq!(r.lo.view().phase, "stopped");
}

#[test]
fn a_pivot_past_the_limit_stops_on_the_ground() {
    // No rate data reaches the pilot, so nothing loads; the probe stops
    // before the pivot, and a pivot past the limit stops in any case.
    let af = Airframe::new(0.6, [0.0, 0.6]);
    let mut r = Run::new(af, None);
    r.lo.loading = [false; 2];
    let mut stopped = None;
    for _ in 0..1500 {
        r.now += 20;
        let mut inp = r.inputs();
        inp.i = None;
        // Hold the attitude past the limit once spooled.
        if r.lo.rest.is_some() {
            inp.att = inp.att.map(|a| Attitude {
                pitch: -(ABORT_DEG + 1.0).to_radians(),
                ..a
            });
        }
        if r.lo.step(&inp).is_none() {
            stopped = r.lo.stopped();
            break;
        }
    }
    assert_eq!(stopped, Some(Stop::Tipping));
}

#[test]
fn a_tip_without_rate_data_stops_instead_of_guessing() {
    let af = Airframe::new(0.6, [0.0, 0.25]);
    let mut r = Run::new(af, None);
    for _ in 0..1500 {
        r.now += 20;
        let mut inp = r.inputs();
        inp.i = None;
        match r.lo.step(&inp) {
            Some(Cmd::Attitude {
                roll,
                pitch,
                yaw,
                thrust,
            }) => r.af.target = Some(([roll, pitch, yaw], thrust)),
            _ => break,
        }
        for _ in 0..4 {
            r.af.step(0.005);
        }
    }
    assert_eq!(r.lo.stopped(), Some(Stop::NoRateData));
    assert!(!r.lo.lifted_ever());
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
    for (at, stops) in [(1000, true), (2500, true), (4500, true)] {
        let mut r = Run::new(Airframe::new(0.6, [0.0, 0.25]), None);
        r.run(at);
        assert!(r.lo.on_ground());
        r.lo.stop(Stop::Operator);
        assert_eq!(r.lo.stopped().is_some(), stops);
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
    assert_eq!(r.lo.view().phase, "spool");
    r.run(1500);
    assert_eq!(r.lo.view().phase, "level");
    assert_eq!(r.lo.view().text, "Levelling on the ground");
    assert_eq!(r.lo.view().trim, "learned");
    let p = Run::new(Airframe::balanced(), None);
    assert_eq!(p.lo.view().trim, "probe");
}
