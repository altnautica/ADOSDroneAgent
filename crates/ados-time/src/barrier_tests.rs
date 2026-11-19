use std::time::Duration;

use ados_protocol::simclock::wire::FLAG_REALTIME;

use super::*;

const MS: u64 = 1_000_000;

fn sim() -> SocketAddr {
    "10.0.0.1:40000".parse().unwrap()
}

fn tick(seq: u32, sim_ms: u64) -> Tick {
    Tick {
        flags: FLAG_LOCKSTEP,
        seq,
        sim_ns: sim_ms * MS,
        grant_ns: (sim_ms + 20) * MS,
        speed: 0.0,
    }
}

#[test]
fn no_participant_acknowledges_at_once() {
    let mut b = Barrier::default();
    let now = Instant::now();
    let (ack, to) = b.tick(&tick(1, 20), sim(), now).unwrap();
    assert_eq!(to, sim());
    assert_eq!(
        ack,
        Ack {
            parts: 0,
            seq: 1,
            done_ns: 40 * MS,
            lag_wall_us: 0
        }
    );
}

#[test]
fn a_slow_participant_holds_the_grant() {
    let mut b = Barrier::default();
    let t0 = Instant::now();
    let pilot = b.join().unwrap();
    let router = b.join().unwrap();
    assert!(b.done(pilot, 20 * MS, t0).is_none(), "no tick waits yet");
    // Done before 20 ms is not done with the work due at 20 ms.
    assert!(b.tick(&tick(1, 20), sim(), t0).is_none());
    assert!(
        b.done(pilot, 40 * MS, t0).is_none(),
        "the router still works"
    );
    let later = t0 + Duration::from_millis(3);
    let (ack, _) = b.done(router, 1_000 * MS, later).unwrap();
    assert_eq!(ack.parts, 2);
    assert_eq!(ack.done_ns, 40 * MS);
    assert_eq!(ack.lag_wall_us, 3_000);
    // The router's report covers the next grant; the pilot's does not.
    assert!(b.tick(&tick(2, 40), sim(), later).is_none());
    assert!(b.done(pilot, 60 * MS, later).is_some());
}

#[test]
fn a_late_participant_is_waited_on() {
    let mut b = Barrier::default();
    let now = Instant::now();
    let pilot = b.join().unwrap();
    b.done(pilot, 60 * MS, now);
    assert!(b.tick(&tick(1, 20), sim(), now).is_some());
    // A plugin registers while the next tick waits.
    let plugin = b.join().unwrap();
    assert!(b.tick(&tick(2, 40), sim(), now).is_none());
    assert!(b.done(plugin, 41 * MS, now).is_some());
    // A participant that leaves stops counting.
    assert!(b.tick(&tick(3, 60), sim(), now).is_none());
    assert!(b.leave(pilot, now).is_none(), "the plugin is not done yet");
    b.done(plugin, 80 * MS, now).unwrap();
    assert_eq!(b.parts(), 1);
}

#[test]
fn a_resent_tick_gets_the_same_ack() {
    let mut b = Barrier::default();
    let now = Instant::now();
    let p = b.join().unwrap();
    assert!(b.tick(&tick(1, 20), sim(), now).is_none());
    assert!(b.tick(&tick(1, 20), sim(), now).is_none(), "still waiting");
    let first = b.done(p, 40 * MS, now).unwrap();
    assert_eq!(b.tick(&tick(1, 20), sim(), now), Some(first));
    // A lower report never moves a participant back.
    b.done(p, 10 * MS, now);
    assert!(b.tick(&tick(2, 30), sim(), now).is_some());
}

#[test]
fn only_running_lockstep_ticks_wait() {
    let mut b = Barrier::default();
    let now = Instant::now();
    b.join().unwrap();
    for flags in [0, FLAG_REALTIME, FLAG_LOCKSTEP | FLAG_PAUSED] {
        let t = Tick {
            flags,
            ..tick(1, 20)
        };
        assert!(b.tick(&t, sim(), now).is_none());
    }
    assert!(b.pending.is_none());
}

#[test]
fn participants_are_capped() {
    let mut b = Barrier::default();
    for _ in 0..MAX_PARTICIPANTS {
        assert!(b.join().is_some());
    }
    assert!(b.join().is_none());
}
