//! The matrix. Each scenario turns a seed into a fault plan and a run; the
//! seed also draws every latency, loss, outage and timing inside the run.

use crate::check::Outcome;
use crate::rng::Rng;
use crate::sim::{simulate, Plan, Run};
use crate::world::{Faults, DRONE, GS};

/// One scenario: a seed in, a run and the outcomes it may end in out.
pub type Scenario = fn(u64) -> (Run, &'static [Outcome]);

/// The scenarios, by name.
pub const ALL: &[(&str, Scenario)] = &[
    ("clean", clean),
    ("lossy", lossy),
    ("no_peer", no_peer),
    ("restart", restart),
    ("cancel", cancel),
    ("concurrent", concurrent),
    ("slow_systemd", slow_systemd),
    ("ground_misses_ok", ground_misses_ok),
    ("unissued_drone", crate::unissued::unissued_drone),
];

/// A fault-free run binds or, with no peer, aborts.
const ANY: &[Outcome] = &[Outcome::Bound, Outcome::Aborted];

/// A fault landing in the last exchange can leave one rig on the new key and
/// the other not, which no protocol can rule out: the drone commits on the
/// ground station's close, the ground station on the drone's `OK`, and the
/// last of those can be lost, cut by a cancel or a kill before the gate, or
/// outlive the sender's tunnel. Every two-rig scenario then runs both rigs'
/// auto-pair loops (the re-arm latch included) for the latch's bound: a split
/// must end bound, a bound pair must stay bound with no window opened on it,
/// and an aborted bind may stay aborted (the station keeps the pairing it had,
/// with its drone away). The matrix line counts the splits reconciled.
const RECONCILED: &[Outcome] = &[Outcome::Bound, Outcome::Aborted];

/// Both rigs bind at about the same time over a clean link: they bind.
fn clean(seed: u64) -> (Run, &'static [Outcome]) {
    let plan = Plan {
        gs_present: true,
        first_bind_s: [(0.0, 5.0), (0.0, 5.0)],
        attempts: [3, 3],
        reconcile: true,
        ..Plan::default()
    };
    (simulate(seed, Faults::clean(), plan), &[Outcome::Bound])
}

/// A lossy, laggy air link with outages, failing bind units and tunnels that
/// never come up.
fn lossy(seed: u64) -> (Run, &'static [Outcome]) {
    let mut r = Rng::new(seed ^ 0x0001_055E);
    let outages = (0..(r.next_u64() % 4))
        .map(|_| {
            (
                std::time::Duration::from_secs_f32(r.range(0.0, 90.0)),
                std::time::Duration::from_secs_f32(r.range(0.5, 25.0)),
            )
        })
        .collect();
    let faults = Faults {
        loss: r.range(0.02, 0.35),
        dup: 0.05,
        latency_s: (0.01, r.range(0.03, 0.25)),
        outages,
        systemctl_s: (0.05, 3.0),
        bind_unit_fail: 0.1,
        iface_never: 0.08,
        ..Faults::clean()
    };
    let plan = Plan {
        gs_present: true,
        first_bind_s: [(0.0, 20.0), (0.0, 20.0)],
        attempts: [4, 4],
        reconcile: true,
        ..Plan::default()
    };
    (simulate(seed, faults, plan), RECONCILED)
}

/// The drone binds with no ground station listening: every attempt ends on
/// a timeout or the watchdog, and the radio is restored each time.
fn no_peer(seed: u64) -> (Run, &'static [Outcome]) {
    let plan = Plan {
        gs_present: false,
        first_bind_s: [(0.0, 5.0), (0.0, 0.0)],
        attempts: [2, 0],
        ..Plan::default()
    };
    (simulate(seed, Faults::clean(), plan), &[Outcome::Aborted])
}

/// One rig's supervisor is killed mid-bind and restarted by systemd; the
/// operators keep trying. On a clean link, so the only fault is the kill: a
/// kill past the killed rig's peer-evidence gate must not leave a split (the
/// restart finishes the apply), an earlier one may and is reconciled.
fn restart(seed: u64) -> (Run, &'static [Outcome]) {
    let rig = if seed.is_multiple_of(2) { DRONE } else { GS };
    let plan = Plan {
        gs_present: true,
        first_bind_s: [(0.0, 5.0), (0.0, 5.0)],
        attempts: [4, 4],
        kill: Some((rig, (1.0, 12.0))),
        reconcile: true,
        ..Plan::default()
    };
    (simulate(seed, Faults::clean(), plan), RECONCILED)
}

/// The operator cancels one rig's bind mid-way and does not retry.
fn cancel(seed: u64) -> (Run, &'static [Outcome]) {
    let rig = if seed.is_multiple_of(2) { DRONE } else { GS };
    let plan = Plan {
        gs_present: true,
        first_bind_s: [(0.0, 5.0), (0.0, 5.0)],
        attempts: [1, 1],
        cancel: Some((rig, (1.0, 12.0))),
        reconcile: true,
        ..Plan::default()
    };
    (simulate(seed, Faults::clean(), plan), RECONCILED)
}

/// A second bind lands on one rig while its first is in flight.
fn concurrent(seed: u64) -> (Run, &'static [Outcome]) {
    let rig = if seed.is_multiple_of(2) { DRONE } else { GS };
    let plan = Plan {
        gs_present: true,
        first_bind_s: [(0.0, 2.0), (0.0, 2.0)],
        attempts: [3, 3],
        second_bind: Some((rig, (0.5, 5.0))),
        reconcile: true,
        ..Plan::default()
    };
    (simulate(seed, Faults::clean(), plan), &[Outcome::Bound])
}

/// A busy service manager: every `systemctl` job takes up to its 30 s bound,
/// so phases run long and the timeouts and the watchdog fire.
fn slow_systemd(seed: u64) -> (Run, &'static [Outcome]) {
    let faults = Faults {
        systemctl_s: (0.5, 30.0),
        ..Faults::clean()
    };
    let plan = Plan {
        gs_present: true,
        first_bind_s: [(0.0, 5.0), (0.0, 5.0)],
        attempts: [2, 2],
        reconcile: true,
        ..Plan::default()
    };
    (simulate(seed, faults, plan), ANY)
}

/// The ground station dies between sending the drone its key and reading the
/// drone's `OK`: the drone commits, the station restarts on the key of its
/// earlier pairing, which once carried a link. Auto-pair must reconcile it
/// with no operator: the drone's new key never works, and the station hears
/// its drone on the air with a key it cannot decode.
fn ground_misses_ok(seed: u64) -> (Run, &'static [Outcome]) {
    let plan = Plan {
        gs_present: true,
        first_bind_s: [(0.0, 5.0), (0.0, 5.0)],
        attempts: [1, 1],
        kill_ground_awaiting_ok: true,
        reconcile: true,
        ..Plan::default()
    };
    let mut run = simulate(seed, Faults::clean(), plan);
    if run.before_reconcile != Outcome::Split {
        run.violations.push(format!(
            "the operators left {:?}, not the drone-ahead split this scenario sets up",
            run.before_reconcile
        ));
    }
    (run, &[Outcome::Bound])
}
