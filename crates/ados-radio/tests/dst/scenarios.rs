//! The scenario matrix: a fault plan per scenario, and what each must have
//! exercised across its seeds for the run to count.

use crate::world::{Faults, Stats};

/// A scenario: its fault plan, and the coverage its seeds must reach together.
pub struct Scenario {
    pub name: &'static str,
    pub faults: fn() -> Faults,
    pub covers: fn(&Stats) -> Result<(), &'static str>,
}

fn need(ok: bool, what: &'static str) -> Result<(), &'static str> {
    if ok {
        Ok(())
    } else {
        Err(what)
    }
}

pub const ALL: &[Scenario] = &[
    Scenario {
        name: "clean",
        faults: Faults::default,
        covers: |s| {
            need(
                s.hops_executed > 0 && s.hop_checks > 0,
                "an executed, checked hop",
            )
        },
    },
    Scenario {
        name: "lossy_acks",
        faults: || Faults {
            loss: 0.15,
            ack_loss: 0.8,
            dup: 0.15,
            delay_ms: (1, 60),
            ..Faults::default()
        },
        covers: |s| {
            need(
                s.acked_unheard > 0 && s.reverts > 0 && s.hop_checks > 0,
                "a hop acked but never heard, its ground's revert, and a checked hop",
            )
        },
    },
    Scenario {
        name: "retune_faults",
        faults: || Faults {
            drone_retune_fail: 0.3,
            drone_spawn_fail: 0.15,
            ground_retune_fail: 0.1,
            ..Faults::default()
        },
        covers: |s| {
            need(
                s.drone_retune_fails > 0 && s.own_channel_announces > 0 && s.reverts > 0,
                "a failed drone retune, its call-back and a ground revert",
            )
        },
    },
    Scenario {
        name: "crashes",
        faults: || Faults {
            crash_every_s: Some((15.0, 50.0)),
            ..Faults::default()
        },
        covers: |s| {
            need(
                s.crashes > 0 && s.rejoin_checks > 0 && s.bringups > s.crashes,
                "crashes with checked rejoins",
            )
        },
    },
    Scenario {
        name: "outages",
        faults: || Faults {
            outage: Some(((40.0, 120.0), (20.0, 70.0))),
            ..Faults::default()
        },
        covers: |s| {
            need(
                s.outages > 0 && s.return_homes > 0,
                "an outage and a return home",
            )
        },
    },
    Scenario {
        name: "two_grounds",
        faults: || Faults {
            grounds: 2,
            loss: 0.1,
            ack_loss: 0.3,
            ..Faults::default()
        },
        covers: |s| {
            need(
                s.hops_executed > 0 && s.sweeps > 0,
                "hops and a stranded ground's sweep",
            )
        },
    },
    Scenario {
        name: "bind_windows",
        faults: || Faults {
            bind: Some(((15.0, 45.0), (5.0, 25.0))),
            manual_every_s: Some((5.0, 20.0)),
            ..Faults::default()
        },
        covers: |s| {
            need(
                s.binds > 0 && s.manual_rejected > 0,
                "bind windows and refused manual hops",
            )
        },
    },
    Scenario {
        name: "manual",
        faults: || Faults {
            manual_every_s: Some((5.0, 15.0)),
            ground_refuses: Some(161),
            ..Faults::default()
        },
        covers: |s| {
            need(
                s.manual_accepted > 0 && s.manual_rejected > 0,
                "accepted and rejected manual hops",
            )
        },
    },
    Scenario {
        name: "periodic",
        faults: || Faults {
            periodic: true,
            loss: 0.4,
            ..Faults::default()
        },
        covers: |s| need(s.hops_announced > 0, "announced hops"),
    },
    Scenario {
        name: "chaos",
        faults: || Faults {
            grounds: 2,
            loss: 0.1,
            ack_loss: 0.2,
            dup: 0.1,
            delay_ms: (1, 40),
            drone_retune_fail: 0.1,
            drone_spawn_fail: 0.05,
            ground_retune_fail: 0.05,
            crash_every_s: Some((40.0, 120.0)),
            outage: Some(((90.0, 200.0), (15.0, 50.0))),
            bind: Some(((60.0, 150.0), (5.0, 20.0))),
            manual_every_s: Some((10.0, 30.0)),
            periodic: true,
            ground_refuses: None,
            interference_every_s: (30.0, 60.0),
        },
        covers: |s| need(s.hops_executed > 0 && s.crashes > 0, "hops and crashes"),
    },
];
