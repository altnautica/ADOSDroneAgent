//! The invariants, checked every tick while the run goes and once at its end.
//!
//! * Agreement: a drone/ground split never lasts longer than
//!   [`SPLIT_BOUND_MS`], whatever the faults.
//! * Hop agreement: 6.5 s after a hop's flip (the follow verify window plus
//!   slack), every ground that acked the hop is on the drone's channel: on the
//!   new one when the drone moved, back on the old one when the drone never
//!   heard the ack. A lost HopAck never strands one side.
//! * Rejoin: a radio group brought up after a crash comes back on the channel
//!   it and its grounds were on (checked at the bring-up, `drone.rs`).
//! * The committed hop state matches the radio's channel within
//!   [`STATE_BOUND_MS`].
//! * No hop or return home starts while a bind owns the adapter, and a manual
//!   hop is never accepted then (checked where they happen).
//! * The live generation's supervisor holds the control-plane return port
//!   within [`PORT_BOUND_MS`] of its bring-up, and answers a manual hop request
//!   within [`MANUAL_BOUND`].
//! * At the end of the quiet tail the pair agrees, and the hop state with it.

use std::sync::Arc;

use ados_radio::radio_cmd::{HopVerdict, ManualHopRequest};
use tokio::sync::oneshot;
use tokio::time::{sleep, Duration};

use crate::drone::Drone;
use crate::world::{lock, Shared, BAND};

/// Longest a drone/ground split may last (a ground that missed every announce
/// waits out the 120 s presence-loss window before it sweeps).
pub const SPLIT_BOUND_MS: u64 = 200_000;
/// A hop's check point after its flip: the 5 s follow verify window and 1.5 s
/// of slack.
const HOP_CHECK_MS: u64 = 5_000 + 1_500;
/// Longest the hop state may disagree with the radio (a stale-peer bring-up
/// on home is corrected by the next 5 s return-home tick).
const STATE_BOUND_MS: u64 = 8_000;
/// The return-port bind retries every 5 s.
const PORT_BOUND_MS: u64 = 6_500;
/// Longest the supervisor may be busy before it answers a manual request: a
/// scan, a hop, a failed retune's 8 s call-back and a return home.
const MANUAL_BOUND: Duration = Duration::from_secs(20);

/// The per-tick monitor.
pub async fn monitor(w: Shared, d: Arc<Drone>) {
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let mut port_flagged_gen = 0;
    loop {
        tick.tick().await;
        let committed = d.state.lock().await.channel;
        let mut g = lock(&w);
        let now = g.now_ms();
        let dch = g.drone.channel;
        let drone_up = g.drone.alive && dch != 0;
        for i in 0..g.grounds.len() {
            let gch = g.grounds[i].channel;
            if !drone_up || gch == 0 {
                continue;
            }
            if gch == dch {
                g.grounds[i].split_since = None;
                continue;
            }
            let since = *g.grounds[i].split_since.get_or_insert(now);
            if now - since > SPLIT_BOUND_MS {
                let name = g.grounds[i].name;
                g.violate(format!(
                    "{name} on {gch} and the drone on {dch} for over {} s",
                    SPLIT_BOUND_MS / 1000
                ));
                g.grounds[i].split_since = Some(now);
            }
        }
        // A bind suppresses the return home that corrects the record, and a
        // failed retune or respawn leaves the record where it was until the
        // next return home lands, so the bound counts from the latest of them.
        if g.bind_open {
            g.state_split_since = None;
        } else if drone_up && !g.drone.respawning && committed != dch {
            let mut since = *g.state_split_since.get_or_insert(now);
            if let Some(t) = g.drone_failed_ms.filter(|&t| t > since) {
                since = t;
                g.state_split_since = Some(t);
            }
            if now - since > STATE_BOUND_MS {
                g.violate(format!(
                    "hop state says {committed} but the radio is on {dch} for over {} s",
                    STATE_BOUND_MS / 1000
                ));
                g.state_split_since = Some(now);
            }
        } else if drone_up {
            g.state_split_since = None;
        }
        let gen = g.drone.generation;
        let live = g.manual_tx.is_some();
        let held = g.port.as_ref().map(|p| p.generation);
        if live
            && now - g.drone.gen_started_ms > PORT_BOUND_MS
            && held != Some(gen)
            && port_flagged_gen != gen
        {
            port_flagged_gen = gen;
            g.violate(format!(
                "supervisor gen {gen} has not bound the return port {} s after bring-up (held by {held:?})",
                PORT_BOUND_MS / 1000
            ));
        }
    }
}

/// Check each hop once its follow verify window has passed.
pub async fn hop_checks(w: Shared) {
    let mut next = 0usize;
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    loop {
        tick.tick().await;
        let mut g = lock(&w);
        let now = g.now_ms();
        while next < g.hops.len() {
            let h = &g.hops[next];
            let due = h.flip_ms + HOP_CHECK_MS;
            if now < due {
                break;
            }
            if h.own_channel || h.acked_by.is_empty() {
                if !h.own_channel {
                    g.stats.hops_unacked += 1;
                }
                next += 1;
                continue;
            }
            let (first, target, executed) = (h.first_ms, h.target, h.executed);
            let acked: Vec<usize> = h.acked_by.iter().copied().collect();
            if !executed {
                g.stats.acked_unheard += 1;
            }
            let superseded = g.hops[next + 1..]
                .iter()
                .any(|l| !l.own_channel && l.first_ms <= due);
            let disrupted = g.disrupted_ms.is_some_and(|t| t + 500 >= first && t <= due)
                || g.outage
                || acked.iter().any(|&i| {
                    g.grounds[i]
                        .retune_failed_ms
                        .is_some_and(|t| t >= first && t <= due)
                });
            if superseded || disrupted {
                g.stats.hop_checks_skipped += 1;
                next += 1;
                continue;
            }
            let dch = g.drone.channel;
            let settled =
                g.drone.alive && dch != 0 && acked.iter().all(|&i| g.grounds[i].channel != 0);
            if !settled {
                if now < due + 3_000 {
                    break;
                }
                g.stats.hop_checks_skipped += 1;
                next += 1;
                continue;
            }
            g.stats.hop_checks += 1;
            for i in acked {
                let gch = g.grounds[i].channel;
                if gch != dch {
                    let name = g.grounds[i].name;
                    let heard = if executed {
                        "flipped"
                    } else {
                        "never heard an ack"
                    };
                    g.violate(format!(
                        "{name} acked the hop to {target} announced at {}.{:03} s and is on {gch}, \
                         the drone ({heard}) on {dch}, after the follow window",
                        first / 1000,
                        first % 1000
                    ));
                }
            }
            next += 1;
        }
    }
}

/// The operator: a manual hop now and then, mostly in band, sometimes not.
pub async fn operator(w: Shared) {
    let Some(every) = lock(&w).f.manual_every_s else {
        return;
    };
    loop {
        let gap = lock(&w).rng.secs(every);
        sleep(Duration::from_millis(gap)).await;
        let (tx, channel) = {
            let mut g = lock(&w);
            if !g.faults_on {
                return;
            }
            let channel = if g.rng.chance(0.25) {
                g.rng.pick(&[36u8, 40])
            } else {
                g.rng.pick(&BAND)
            };
            (g.manual_tx.clone(), channel)
        };
        let Some(tx) = tx else {
            continue;
        };
        let (reply, rx) = oneshot::channel();
        if tx.try_send(ManualHopRequest { channel, reply }).is_err() {
            lock(&w).log(format!("operator hop {channel}: unavailable"));
            continue;
        }
        let verdict = tokio::time::timeout(MANUAL_BOUND, rx).await;
        let mut g = lock(&w);
        match verdict {
            Ok(Ok(HopVerdict::Accepted { channel })) => {
                g.stats.manual_accepted += 1;
                g.log(format!("operator hop {channel}: accepted"));
                if g.bind_open {
                    g.violate(format!(
                        "manual hop to {channel} accepted while a bind owns the adapter"
                    ));
                }
            }
            Ok(Ok(HopVerdict::Rejected { reason })) => {
                g.stats.manual_rejected += 1;
                g.log(format!("operator hop {channel}: rejected ({reason})"));
            }
            Ok(Err(_)) => g.log(format!("operator hop {channel}: dropped by a respawn")),
            Err(_) => g.violate(format!(
                "manual hop request to {channel} unanswered for {} s",
                MANUAL_BOUND.as_secs()
            )),
        }
    }
}

/// The end of the quiet tail: the radio is up, every ground is with it, and
/// the hop state says where it is.
pub async fn finish(w: &Shared, d: &Drone) {
    let committed = d.state.lock().await.channel;
    let mut g = lock(w);
    let dch = g.drone.channel;
    if !g.drone.alive || dch == 0 {
        g.violate(format!(
            "the drone radio is down at the end (channel {dch})"
        ));
    }
    for i in 0..g.grounds.len() {
        let gch = g.grounds[i].channel;
        if gch != dch {
            let name = g.grounds[i].name;
            g.violate(format!("{name} ends on {gch}, the drone on {dch}"));
        }
    }
    if committed != dch {
        g.violate(format!(
            "the hop state ends on {committed}, the radio on {dch}"
        ));
    }
    g.log(format!("end: drone on {dch}, hop state {committed}"));
}
