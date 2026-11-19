//! Characterization pins for goals, holds and landings flown closed loop:
//! the simulated copter's position, velocity and heading every 200 ms of a
//! goto with its arrival hold, a goto the sticks take over, a stick hold,
//! and the pilot's landing with and without an operator stick session, on
//! three seeds each, compared with their recordings ([`crate::pins::Pin`]).

use ados_protocol::pilot::{Owner, Request};

use crate::pins::Pin;
use crate::scenarios::{fly_sticks, launch_to_hold};
use crate::sim::Sim;

/// The pins are recorded to six decimals and the simulation replays a seed
/// byte for byte: a pure restructuring of the pilot repeats them to the
/// last digit. One millimetre (1 mm/s, 1 mrad) leaves room for a reordered
/// floating-point sum, far below the seeded wind and sensor noise the
/// copter already flies in.
const TOL: f32 = 1e-3;

const SEEDS: [u64; 3] = [1, 2, 3];

const COLS: &[&str] = &["n", "e", "d", "vn", "ve", "vd", "yaw"];
/// Sampling period, ms.
const EVERY_MS: u64 = 200;

/// Sample the copter now.
fn sample(pin: &mut Pin, s: &Sim, t0: u64) {
    let f = &s.fc;
    pin.row(
        s.now - t0,
        &[
            f.pos[0], f.pos[1], f.pos[2], f.vel[0], f.vel[1], f.vel[2], f.att[2],
        ],
    );
}

/// Run `ms`, `each(sim, ms since t0)` before every tick, sampling every
/// [`EVERY_MS`]; stops early once `done` holds.
fn drive(
    s: &mut Sim,
    pin: &mut Pin,
    t0: u64,
    ms: u64,
    mut each: impl FnMut(&mut Sim, u64),
    done: impl Fn(&Sim) -> bool,
) {
    let end = s.now + ms;
    while s.now < end && !done(s) {
        each(s, s.now - t0);
        s.step();
        if (s.now - t0).is_multiple_of(EVERY_MS) {
            sample(pin, s, t0);
        }
    }
}

fn check(pin: &Pin, name: &str) {
    let worst = pin.check(TOL);
    eprintln!("dst pin {name}: within {worst:.6} of its recording");
}

fn goto(n: f32, e: f32, d: f32) -> Request {
    Request::Goto { n, e, d, yaw: None }
}

/// From the launch hold (no stick session), a goto 14 m away and 2 m up:
/// the goal's shaped approach, the arrival and the pilot's hold after it.
#[test]
fn dst_pin_goto_and_arrival_hold() {
    for seed in SEEDS {
        let mut s = Sim::new("pin_goto", seed, |_| {});
        s.boot();
        launch_to_hold(&mut s, 5.0);
        s.request(goto(12.0, -8.0, -7.0));
        assert_eq!(s.p.owner(), Owner::Goto);
        let name = format!("dst-goto-s{seed}");
        let mut pin = Pin::new(&name, COLS);
        let t0 = s.now;
        drive(&mut s, &mut pin, t0, 25_000, |_, _| {}, |_| false);
        assert_eq!(s.p.owner(), Owner::Pilot, "arrived and holds");
        check(&pin, &name);
    }
}

/// A goto 30 m north; 3 s in the operator pushes the roll stick and flies
/// on from the goal's reference, then releases it to a stick hold.
#[test]
fn dst_pin_sticks_take_over_a_goto() {
    for seed in SEEDS {
        let mut s = Sim::new("pin_takeover", seed, |_| {});
        s.boot();
        launch_to_hold(&mut s, 5.0);
        s.sticks = Some([0.0; 4]);
        s.run(800);
        s.request(goto(30.0, 0.0, -5.0));
        let name = format!("dst-takeover-s{seed}");
        let mut pin = Pin::new(&name, COLS);
        let t0 = s.now;
        drive(
            &mut s,
            &mut pin,
            t0,
            12_000,
            |s, t| {
                s.sticks = Some(if (3_000..5_000).contains(&t) {
                    [0.6, 0.0, 0.0, 0.0]
                } else {
                    [0.0; 4]
                });
            },
            |_| false,
        );
        assert_eq!(s.p.owner(), Owner::Pilot, "the sticks took over");
        check(&pin, &name);
    }
}

/// Sticks forward, right, up and turning for 3 s, then released: the stick
/// reference brakes to a hold on every axis and holds there.
#[test]
fn dst_pin_stick_release_hold() {
    for seed in SEEDS {
        let mut s = Sim::new("pin_hold", seed, |_| {});
        s.boot();
        launch_to_hold(&mut s, 5.0);
        s.sticks = Some([0.0; 4]);
        s.run(800);
        let name = format!("dst-hold-s{seed}");
        let mut pin = Pin::new(&name, COLS);
        let t0 = s.now;
        drive(
            &mut s,
            &mut pin,
            t0,
            10_000,
            |s, t| {
                s.sticks = Some(if t < 3_000 {
                    [0.4, 0.8, 0.3, 0.3]
                } else {
                    [0.0; 4]
                });
            },
            |_| false,
        );
        check(&pin, &name);
    }
}

/// The pilot's landing, `session` with the operator's (centred) sticks
/// streaming or without: it descends, hands over to the FC's LAND and
/// disarms on the ground.
fn landing(session: bool) {
    let tag = if session { "sticks" } else { "hold" };
    for seed in SEEDS {
        let mut s = Sim::new("pin_landing", seed, |_| {});
        s.boot();
        launch_to_hold(&mut s, 5.0);
        if session {
            fly_sticks(&mut s, [0.0, 0.6, 0.0, 0.0], 2_000);
        }
        assert!(s.action("land", None).ok, "land");
        let name = format!("dst-landing-{tag}-s{seed}");
        let mut pin = Pin::new(&name, COLS);
        let t0 = s.now;
        drive(&mut s, &mut pin, t0, 40_000, |_, _| {}, |s| !s.fc.armed);
        assert!(!s.fc.armed, "landed and disarmed");
        check(&pin, &name);
    }
}

#[test]
fn dst_pin_landing_with_a_stick_session() {
    landing(true);
}

#[test]
fn dst_pin_landing_without_a_stick_session() {
    landing(false);
}
