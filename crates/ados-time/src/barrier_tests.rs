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
            lag_wall_us: 0,
            flags: 0,
        }
    );
}

#[test]
fn a_slow_participant_holds_the_grant() {
    let mut b = Barrier::default();
    let t0 = Instant::now();
    let pilot = b.join().unwrap();
    let router = b.join().unwrap();
    assert!(b.done(pilot, 20 * MS, 0, t0).is_none(), "no tick waits yet");
    // Done before 20 ms is not done with the work due at 20 ms.
    assert!(b.tick(&tick(1, 20), sim(), t0).is_none());
    assert!(
        b.done(pilot, 40 * MS, 0, t0).is_none(),
        "the router still works"
    );
    let later = t0 + Duration::from_millis(3);
    let (ack, _) = b.done(router, 1_000 * MS, 0, later).unwrap();
    assert_eq!(ack.parts, 2);
    assert_eq!(ack.done_ns, 40 * MS);
    assert_eq!(ack.lag_wall_us, 3_000);
    // The router's report covers the next grant; the pilot's does not.
    assert!(b.tick(&tick(2, 40), sim(), later).is_none());
    assert!(b.done(pilot, 60 * MS, 0, later).is_some());
}

#[test]
fn a_late_participant_is_waited_on() {
    let mut b = Barrier::default();
    let now = Instant::now();
    let pilot = b.join().unwrap();
    b.done(pilot, 60 * MS, 0, now);
    assert!(b.tick(&tick(1, 20), sim(), now).is_some());
    // A plugin registers while the next tick waits.
    let plugin = b.join().unwrap();
    assert!(b.tick(&tick(2, 40), sim(), now).is_none());
    assert!(b.done(plugin, 41 * MS, 0, now).is_some());
    // A participant that leaves stops counting.
    assert!(b.tick(&tick(3, 60), sim(), now).is_none());
    assert!(b.leave(pilot, now).is_none(), "the plugin is not done yet");
    b.done(plugin, 80 * MS, 0, now).unwrap();
    assert_eq!(b.parts(), 1);
}

#[test]
fn a_resent_tick_gets_the_same_ack() {
    let mut b = Barrier::default();
    let now = Instant::now();
    let p = b.join().unwrap();
    assert!(b.tick(&tick(1, 20), sim(), now).is_none());
    assert!(b.tick(&tick(1, 20), sim(), now).is_none(), "still waiting");
    let first = b.done(p, 40 * MS, 0, now).unwrap();
    assert_eq!(b.tick(&tick(1, 20), sim(), now), Some(first));
    // A lower report never moves a participant back.
    b.done(p, 10 * MS, 0, now);
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

#[test]
fn the_start_flags_follow_the_gates() {
    // No gate: never ready (the run's start waits for the router's FC link,
    // so a pilot alone does not start it).
    assert_eq!(start_flags([0, 0]), 0);
    assert_eq!(start_flags([]), 0);
    // A detached router (FC transport closed) is neither.
    assert_eq!(start_flags([0, DONE_GATE]), 0);
    assert_eq!(start_flags([0, DONE_GATE | DONE_ATTACHED]), ACK_ATTACHED);
    assert_eq!(
        start_flags([DONE_GATE | DONE_ATTACHED | DONE_READY]),
        ACK_ATTACHED | ACK_READY
    );
    // Every gate must be ready.
    assert_eq!(
        start_flags([DONE_GATE | DONE_ATTACHED | DONE_READY, DONE_GATE | DONE_ATTACHED]),
        ACK_ATTACHED
    );
}

#[test]
fn a_held_start_sees_the_router_attach_and_hear_its_fc_on_the_resent_tick() {
    let mut b = Barrier::default();
    let now = Instant::now();
    let pilot = b.join().unwrap();
    let router = b.join().unwrap();
    b.done(pilot, 2_000 * MS, 0, now);
    // The router lane is done but its FC target is closed (a detached agent).
    assert!(b.done(router, 2_000 * MS, DONE_GATE, now).is_none(), "no tick waits yet");
    let (ack, _) = b.tick(&tick(1, 1_000), sim(), now).unwrap();
    assert_eq!(ack.flags, 0);
    // Configured: its FC transport opens while the simulator holds the tick.
    b.done(router, 2_000 * MS, DONE_GATE | DONE_ATTACHED, now);
    assert_eq!(b.tick(&tick(1, 1_000), sim(), now).unwrap().0.flags, ACK_ATTACHED);
    // The FC's HEARTBEAT heard: the next ACK says ready.
    b.done(router, 2_000 * MS, DONE_GATE | DONE_ATTACHED | DONE_READY, now);
    let (ack, _) = b.tick(&tick(2, 1_020), sim(), now).unwrap();
    assert_eq!(ack.flags, ACK_ATTACHED | ACK_READY);
    // The router restarting drops the gate: not ready until it is back.
    b.leave(router, now);
    assert_eq!(b.flags(), 0);
}
