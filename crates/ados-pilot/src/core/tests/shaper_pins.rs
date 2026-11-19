//! Characterization pins for the shaped reference in the test world: a
//! skill goal, a goto with its arrival hold, a goto braked to a blocked
//! hold, and sticks taking over a goal and releasing to a hold. Each flight
//! is sampled every 100 ms (the vehicle and the setpoint sent) and compared
//! with its recording ([`super::pins::Pin`]).

use ados_protocol::pilot::{Goal, GoalKind, Request};

use super::harness::{wall, World};
use super::pins::Pin;
use super::shaping::flying;
use crate::modes::copter;

/// The pins are recorded to six decimals and the shaping arithmetic is
/// deterministic on one machine: a pure restructuring repeats them to the
/// last digit. Across machines the platform's `sin`/`cos` can differ in the
/// last bit, and a closed loop carries that to about a millimetre over a few
/// seconds (1.2 mm at 4.3 s on CI). One centimetre (1 cm/s, 10 mrad) allows
/// for that and nothing a flight would show.
const TOL: f32 = 1e-2;

/// The vehicle's horizontal position and velocity, and the last setpoint:
/// its position, velocity, acceleration and heading.
const COLS: &[&str] = &[
    "n", "e", "vn", "ve", "sp_n", "sp_e", "sp_d", "sp_vn", "sp_ve", "sp_vd", "sp_an", "sp_ae",
    "sp_yaw",
];

/// Run `ms` in 20 ms ticks, `each(world, ms since start)` before every tick,
/// sampling every 100 ms.
fn drive(w: &mut World, pin: &mut Pin, ms: u64, mut each: impl FnMut(&mut World, u64)) {
    let t0 = w.now;
    while w.now < t0 + ms {
        each(w, w.now - t0);
        w.run(20);
        if (w.now - t0).is_multiple_of(100) {
            let sp = w.last_setpoint().cloned().expect("a setpoint was sent");
            pin.row(
                w.now - t0,
                &[
                    w.pos[0], w.pos[1], w.vel[0], w.vel[1], sp.x, sp.y, sp.z, sp.vx, sp.vy, sp.vz,
                    sp.afx, sp.afy, sp.yaw,
                ],
            );
        }
    }
}

fn check(pin: &Pin) {
    let worst = pin.check(TOL);
    eprintln!("pin within {worst:.6} of its recording");
}

/// A skill goal moving north at 1.2 m/s with its feed-forward, 6 m north
/// and 4 m east of the start, then standing still: the reference speeds up,
/// follows, turns toward it and stops into it on the goal limits.
#[test]
fn pin_a_moving_goal_and_its_stop() {
    let (mut w, id) = flying();
    let mut pin = Pin::new("core-goal", COLS);
    drive(&mut w, &mut pin, 16_000, |w, t| {
        if t.is_multiple_of(100) {
            let s = (t as f32 / 1000.0).min(8.0);
            let mut g = Goal::new(GoalKind::Position, [6.0 + 1.2 * s, 4.0, -5.0], 5.0);
            g.vn = if t < 8_000 { 1.2 } else { 0.0 };
            w.req(Request::Goal {
                lease_id: id,
                goal: g,
            });
        }
    });
    check(&pin);
}

/// A goto 15 m away flies the shaped approach, arrives and hands over to
/// the pilot's hold.
#[test]
fn pin_a_goto_and_its_arrival_hold() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.fly = true;
    w.req(Request::Goto {
        n: 12.0,
        e: -9.0,
        d: -5.0,
        yaw: None,
    });
    let mut pin = Pin::new("core-goto", COLS);
    drive(&mut w, &mut pin, 20_000, |_, _| {});
    check(&pin);
}

/// A goto north in open space; 3 s in, a wall shows up 12 m north of the
/// start: the plan is blocked, the goal's reference slows to a stop on the
/// goal limits and holds where it ends.
#[test]
fn pin_a_goto_braked_to_a_blocked_hold() {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.fly = true;
    w.req(Request::Goto {
        n: 30.0,
        e: 0.0,
        d: -5.0,
        yaw: None,
    });
    let mut pin = Pin::new("core-blocked", COLS);
    drive(&mut w, &mut pin, 12_000, |w, t| {
        if t >= 3_000 {
            w.scan_ranges = wall(0, 12.0 - w.pos[0]);
        }
    });
    check(&pin);
}

/// A skill goal cruising north; the operator holds the roll stick over for
/// the takeover and on, then releases it: the sticks fly on from the goal's
/// reference and brake to a hold on the stick limits.
#[test]
fn pin_sticks_taking_over_a_goal_then_holding() {
    let (mut w, id) = flying();
    let mut pin = Pin::new("core-takeover", COLS);
    drive(&mut w, &mut pin, 12_000, |w, t| {
        if t < 6_000 && t.is_multiple_of(100) {
            let mut g = Goal::new(GoalKind::Position, [40.0, 0.0, -5.0], 3.0);
            g.vn = 3.0;
            w.req(Request::Goal {
                lease_id: id,
                goal: g,
            });
        }
        if (3_000..7_000).contains(&t) {
            w.input([0.95, 0.0, 0.0, 0.0]);
        } else if t >= 7_000 {
            w.input([0.0; 4]);
        }
    });
    check(&pin);
}
