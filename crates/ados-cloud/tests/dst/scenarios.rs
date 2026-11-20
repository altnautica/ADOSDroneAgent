//! The matrix. Each scenario turns a seed into an operator plan and a fault
//! plan; the seed also draws every latency, loss, duplicate and handler time
//! inside the run.

use serde_json::{json, Value};

use crate::rng::Rng;
use crate::sim::{simulate, Plan, Run};
use crate::world::{Away, Enqueue, Faults, Outage};

pub type Scenario = fn(u64) -> Run;

pub const ALL: &[(&str, Scenario)] = &[
    ("clean", clean),
    ("lossy", lossy),
    ("outage", outage),
    ("restart", restart),
    ("slow_batch", slow_batch),
    ("storm", storm),
    ("long_offline", long_offline),
    ("poisoned_ack", poisoned_ack),
];

/// Operators are active for this long.
const ACTIVE_MS: u64 = 20 * 60 * 1000;

fn base() -> Faults {
    Faults {
        latency_ms: (20, 250),
        restart_ms: (2_000, 15_000),
        install_ms: (2_000, 15_000),
        p_handler_fails: 0.1,
        ..Faults::default()
    }
}

fn pick<'a, T>(r: &mut Rng, xs: &'a [T]) -> &'a T {
    &xs[(r.next_u64() % xs.len() as u64) as usize]
}

/// One command with its args; `self_restart` lets a restart name the cloud
/// relay's own unit.
fn command(r: &mut Rng, n: usize, self_restart: bool) -> (&'static str, Value) {
    let plugin = json!({"pluginId": "com.example.dst", "jobId": format!("j-{n}")});
    match r.next_u64() % 14 {
        0 => ("get_services", Value::Null),
        1 => ("get_peers", Value::Null),
        2 => ("get_enrollment", Value::Null),
        3 => ("get_logs", json!({"limit": 50})),
        4 => ("get_peripherals", Value::Null),
        5 => ("scan_peripherals", Value::Null),
        6 => {
            let unit = if self_restart && r.unit() < 0.5 {
                "ados-cloud"
            } else {
                "ados-video"
            };
            ("restart_service", json!({"name": unit}))
        }
        7 => (
            *pick(
                r,
                &[
                    "wfb_pair_init_remote",
                    "wfb_pair_apply_remote",
                    "wfb_pair_unpair",
                ],
            ),
            json!({"role": "drone"}),
        ),
        8 => ("plugin.install", plugin),
        9 => (
            *pick(r, &["plugin.enable", "plugin.disable", "plugin.uninstall"]),
            plugin,
        ),
        _ => (
            "send_command",
            json!({"cmd": *pick(r, &["arm", "takeoff", "land", "rtl", "mode"]), "args": []}),
        ),
    }
}

/// A delivery window: flight commands mostly the default, the rest mostly
/// none, some with an explicit TTL.
fn ttl(r: &mut Rng, name: &str) -> Option<u64> {
    let p = if name == "send_command" { 0.5 } else { 0.25 };
    (r.unit() < p).then(|| 1_000 + r.next_u64() % 59_000)
}

/// `n` bursts of 1-4 commands over the active window.
fn operators(r: &mut Rng, bursts: usize, self_restart: bool) -> Vec<Enqueue> {
    let mut out = Vec::new();
    for _ in 0..bursts {
        let at = r.next_u64() % (ACTIVE_MS - 60_000);
        for k in 0..(1 + r.next_u64() % 4) {
            let (command, args) = command(r, out.len(), self_restart);
            out.push(Enqueue {
                at_ms: at + k * (r.next_u64() % 300),
                command,
                ttl_ms: ttl(r, command),
                args,
                poisoned: false,
            });
        }
    }
    out
}

/// `min` to `min + 2` outages of 10-180 s inside the active window.
fn outages(r: &mut Rng, min: u64) -> Vec<Outage> {
    let n = min + r.next_u64() % 3;
    (0..n)
        .map(|_| {
            let start_ms = 30_000 + r.next_u64() % (ACTIVE_MS - 240_000);
            Outage {
                start_ms,
                end_ms: start_ms + 10_000 + r.next_u64() % 170_000,
                blackhole: r.unit() < 0.5,
            }
        })
        .collect()
}

/// Planned kills every 40-200 s of the active window.
fn kills(r: &mut Rng) -> Vec<u64> {
    let mut out = Vec::new();
    let mut t = 0;
    loop {
        t += 40_000 + r.next_u64() % 160_000;
        if t >= ACTIVE_MS {
            return out;
        }
        out.push(t);
    }
}

fn plan(enqueues: Vec<Enqueue>) -> Plan {
    Plan {
        enqueues,
        active_ms: ACTIVE_MS,
    }
}

/// No faults: every command runs once and is acked, and is claimed promptly.
fn clean(seed: u64) -> Run {
    let mut r = Rng::new(seed ^ 0xC1EA);
    let ops = operators(&mut r, 30, false);
    let faults = Faults {
        live_claims: true,
        ..base()
    };
    simulate(seed, faults, plan(ops))
}

/// Lost requests and answers, slow links, rows duplicated and reordered in a
/// claim, acks delivered twice.
fn lossy(seed: u64) -> Run {
    let mut r = Rng::new(seed ^ 0x1055);
    let faults = Faults {
        latency_ms: (20, 800),
        p_req_lost: 0.1,
        p_resp_lost: 0.1,
        p_dup_row: 0.15,
        p_reorder: 0.3,
        p_dup_ack: 0.1,
        ..base()
    };
    simulate(seed, faults, plan(operators(&mut r, 30, false)))
}

/// The backend goes away (refusing, or swallowing requests) for 10-180 s at a
/// time, with commands in flight.
fn outage(seed: u64) -> Run {
    let mut r = Rng::new(seed ^ 0x0A7A);
    let faults = Faults {
        outages: outages(&mut r, 2),
        ..base()
    };
    simulate(seed, faults, plan(operators(&mut r, 40, false)))
}

/// The node is killed mid-pass and restarts, sometimes after its leases
/// expire; restarts of its own unit kill it mid-command.
fn restart(seed: u64) -> Run {
    let mut r = Rng::new(seed ^ 0x8E57);
    let faults = Faults {
        kills: kills(&mut r),
        restart_ms: (1_000, 75_000),
        ..base()
    };
    simulate(seed, faults, plan(operators(&mut r, 40, true)))
}

/// A long plugin install ahead of short-window flight commands in one claim.
fn slow_batch(seed: u64) -> Run {
    let mut r = Rng::new(seed ^ 0x5B47);
    let mut ops = operators(&mut r, 15, false);
    for _ in 0..6 {
        let at = r.next_u64() % (ACTIVE_MS - 120_000);
        let n = ops.len();
        ops.push(Enqueue {
            at_ms: at,
            command: "plugin.install",
            args: json!({"pluginId": "com.example.dst", "jobId": format!("j-{n}")}),
            ttl_ms: None,
            poisoned: false,
        });
        for k in 0..(1 + r.next_u64() % 3) {
            ops.push(Enqueue {
                at_ms: at + 1 + k,
                command: "send_command",
                args: json!({"cmd": "land", "args": []}),
                ttl_ms: None,
                poisoned: false,
            });
        }
    }
    let faults = Faults {
        install_ms: (20_000, 90_000),
        ..base()
    };
    simulate(seed, faults, plan(ops))
}

/// Everything at once.
fn storm(seed: u64) -> Run {
    let mut r = Rng::new(seed ^ 0x5707);
    let faults = Faults {
        latency_ms: (20, 800),
        p_req_lost: 0.05,
        p_resp_lost: 0.05,
        p_dup_row: 0.1,
        p_reorder: 0.3,
        p_dup_ack: 0.05,
        outages: outages(&mut r, 1),
        kills: kills(&mut r),
        restart_ms: (1_000, 75_000),
        install_ms: (2_000, 60_000),
        p_handler_fails: 0.1,
        away: None,
        live_claims: false,
    };
    simulate(seed, faults, plan(operators(&mut r, 40, true)))
}

/// A node that goes away for 8 days holding undelivered acks: the backend
/// loses it mid-batch (refusing every request), it dies, and while it is
/// away the stuck sweep fails its rows and the retention sweep deletes them.
/// Back, its held acks answer 404 and it takes new commands at once.
fn long_offline(seed: u64) -> Run {
    let mut r = Rng::new(seed ^ 0x0FF1);
    let at = 8 * 60_000 + r.next_u64() % 60_000;
    let mut ops: Vec<Enqueue> = operators(&mut r, 25, false)
        .into_iter()
        .filter(|e| e.command != "plugin.install")
        .filter(|e| e.at_ms + 90_000 < at || e.at_ms > at + 60_000)
        .collect();
    // One batch whose acks land in the outage: claimed within a poll of
    // at - 60 s, its install runs at least 30 s, past the outage's start at
    // at - 40 s, and the two after it follow.
    for (k, command) in ["plugin.install", "get_services", "scan_peripherals"]
        .into_iter()
        .enumerate()
    {
        ops.push(Enqueue {
            at_ms: at - 60_000 + k as u64,
            command,
            args: json!({"pluginId": "com.example.dst", "jobId": "j-away"}),
            ttl_ms: None,
            poisoned: false,
        });
    }
    let faults = Faults {
        outages: vec![Outage {
            start_ms: at - 40_000,
            end_ms: at + 5_000,
            blackhole: false,
        }],
        away: Some(Away {
            at_ms: at,
            server_ms: 8 * 24 * 60 * 60 * 1000,
        }),
        install_ms: (30_000, 35_000),
        live_claims: true,
        ..base()
    };
    simulate(seed, faults, plan(ops))
}

/// One command in ten has an ack the backend fails on every time (500): those
/// acks are given up on, and never hold the other commands' claims for long.
fn poisoned_ack(seed: u64) -> Run {
    let mut r = Rng::new(seed ^ 0x9015);
    let mut ops = operators(&mut r, 30, false);
    for e in &mut ops {
        e.poisoned = r.unit() < 0.1;
    }
    let faults = Faults {
        live_claims: true,
        ..base()
    };
    simulate(seed, faults, plan(ops))
}
