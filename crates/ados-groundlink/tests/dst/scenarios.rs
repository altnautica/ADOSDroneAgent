//! The handovers of the matrix. Each takes the run's generator and returns
//! the grounds, the faults and the script; a seed varies every time in the
//! script, every delay and loss on the LAN and the radio, and every stop and
//! open time of a transmitter.

use crate::rng::Rng;
use crate::world::{Action, Faults, Plan};

/// One scenario: a seed's generator to its plan.
pub type Scenario = fn(&mut Rng) -> Plan;

/// The scenarios, by name.
pub const ALL: &[(&str, Scenario)] = &[
    ("handover", handover),
    ("take_back", take_back),
    ("silent_holder", silent_holder),
    ("tie_three_grounds", tie_three_grounds),
    ("replay_restart", replay_restart),
    ("take_back_race", take_back_race),
];

/// Device ids longer than a beacon's 16 bytes, so the box matches its holder
/// on the truncated id.
fn phones(n: usize) -> Vec<String> {
    ["pixel8-a1b2c3d4e5f6a7b8", "galaxy-0f1e2d3c4b5a6978"][..n]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// A script whose moves follow each other by seeded gaps.
struct Script<'r> {
    rng: &'r mut Rng,
    t: u64,
    moves: Vec<(u64, Action)>,
}

impl<'r> Script<'r> {
    fn new(rng: &'r mut Rng) -> Self {
        Self {
            rng,
            t: 0,
            moves: Vec::new(),
        }
    }

    /// `a` after a gap of `lo..=hi` seconds.
    fn after(mut self, lo: u64, hi: u64, a: Action) -> Self {
        self.t += self.rng.ms(lo * 1_000, hi * 1_000);
        self.moves.push((self.t, a));
        self
    }

    fn done(self) -> Vec<(u64, Action)> {
        self.moves
    }
}

/// Take and give back, three times, over a lossy LAN that also repeats
/// requests; the last take keeps the uplink.
fn handover(rng: &mut Rng) -> Plan {
    let script = Script::new(rng)
        .after(2, 6, Action::Take(0))
        .after(30, 70, Action::GiveBack(0))
        .after(10, 20, Action::Take(0))
        .after(30, 60, Action::GiveBack(0))
        .after(5, 15, Action::Take(0))
        .done();
    Plan {
        phones: phones(1),
        faults: Faults {
            lan_loss: 0.05,
            lan_dup: 0.05,
            radio_loss: 0.05,
            replay: 0.002,
        },
        script,
    }
}

/// The box's operator takes the uplink back from a holding phone, with the
/// take-back call or its answer sometimes lost.
fn take_back(rng: &mut Rng) -> Plan {
    let script = Script::new(rng)
        .after(2, 6, Action::Take(0))
        .after(20, 50, Action::TakeBack)
        .after(10, 20, Action::Take(0))
        .after(20, 50, Action::TakeBack)
        .after(10, 20, Action::Take(0))
        .after(20, 40, Action::GiveBack(0))
        .done();
    Plan {
        phones: phones(1),
        faults: Faults {
            lan_loss: 0.1,
            lan_dup: 0.05,
            radio_loss: 0.05,
            replay: 0.0,
        },
        script,
    }
}

/// The holder goes silent: one phone crashes, then another keeps
/// transmitting while the box stops hearing it, so the box falls back after
/// the stale window and the tie rule stops the phone.
fn silent_holder(rng: &mut Rng) -> Plan {
    let script = Script::new(rng)
        .after(2, 6, Action::Take(0))
        .after(15, 40, Action::Crash(0))
        .after(40, 50, Action::Take(1))
        .after(10, 20, Action::Fade(1, 45_000))
        .after(60, 70, Action::Take(1))
        .done();
    Plan {
        phones: phones(2),
        faults: Faults {
            lan_loss: 0.05,
            lan_dup: 0.0,
            radio_loss: 0.1,
            replay: 0.0,
        },
        script,
    }
}

/// Three grounds: a second phone tries to take while the first holds, takes
/// after the first gives back, and loses the uplink to the box's restart.
fn tie_three_grounds(rng: &mut Rng) -> Plan {
    let script = Script::new(rng)
        .after(2, 6, Action::Take(0))
        .after(5, 15, Action::Take(1))
        .after(20, 40, Action::GiveBack(0))
        .after(30, 40, Action::Take(1))
        .after(20, 40, Action::RestartBox)
        .after(40, 50, Action::Take(1))
        .after(5, 10, Action::Take(0))
        .done();
    Plan {
        phones: phones(2),
        faults: Faults {
            lan_loss: 0.05,
            lan_dup: 0.05,
            radio_loss: 0.05,
            replay: 0.0,
        },
        script,
    }
}

/// An eavesdropper replays every captured take, give-back and take-back call,
/// across restarts of the control service (its replay guard reloads from
/// disk) and of the radio service.
fn replay_restart(rng: &mut Rng) -> Plan {
    let script = Script::new(rng)
        .after(2, 6, Action::Take(0))
        .after(15, 30, Action::GiveBack(0))
        .after(10, 15, Action::RestartControl)
        .after(5, 10, Action::Take(0))
        .after(15, 30, Action::TakeBack)
        .after(5, 10, Action::RestartBox)
        .after(10, 15, Action::Take(0))
        .after(15, 30, Action::RestartControl)
        .after(5, 10, Action::GiveBack(0))
        .done();
    Plan {
        phones: phones(1),
        faults: Faults {
            lan_loss: 0.03,
            lan_dup: 0.1,
            radio_loss: 0.03,
            replay: 0.03,
        },
        script,
    }
}

/// The box's operator takes the uplink back just as the phone's take
/// completes, while the phone opens its transmitters.
fn take_back_race(rng: &mut Rng) -> Plan {
    let mut s = Script::new(rng);
    for _ in 0..4 {
        s = s.after(5, 10, Action::Take(0));
        let gap = s.rng.ms(0, 1_500);
        s.t += gap;
        s.moves.push((s.t, Action::TakeBack));
        s = s.after(30, 40, Action::GiveBack(0));
    }
    Plan {
        phones: phones(1),
        faults: Faults {
            lan_loss: 0.0,
            lan_dup: 0.0,
            radio_loss: 0.0,
            replay: 0.0,
        },
        script: s.done(),
    }
}
