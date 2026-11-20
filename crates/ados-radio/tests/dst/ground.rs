//! A ground station, modelled on `ados-groundlink`'s hop follower
//! (`presence/follow.rs`), presence cache (`presence/cache.rs`) and
//! valid-packet watchdog with its channel acquirer (`watchdog/mod.rs`,
//! `acquire.rs`); the constants and branch order are theirs. The frames are the
//! real wire: announces are verified with `parse_hop_announce` and echoed
//! verbatim as the HopAck, beacons are built and parsed with the radio's own
//! codec.
//!
//! The ground side is a model, not the code: `ados-groundlink` depends on this
//! crate, and its follower is private to its presence module. One deliberate
//! difference: a failed follow reverts to the channel the radio was on when
//! the follow was claimed, the "channel being left" the follower documents.
//! The follower itself reads the presence cache's hint, which a watchdog sweep
//! or a drone beacon can have moved, and then reverts to a channel nobody is
//! on: with the hint, `retune_faults` and `chaos` seed 12 leave a ground split
//! from the drone until its watchdog sweeps or the drone returns home.

use ados_radio::channel::standard_channels;
use ados_radio::hop::{
    build_presence_beacon, hop_announce_delay_ms, parse_hop_announce, parse_presence_beacon,
};
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration};

use crate::world::{key, lock, transmit, Node, Shared, World, HOME};

/// `follow.rs` `MAX_FOLLOW_WAIT`.
const MAX_FOLLOW_WAIT_MS: u64 = 3000;
/// `follow.rs` `FOLLOW_VERIFY_WINDOW`.
const FOLLOW_VERIFY_MS: u64 = 5000;
/// `presence.rs` `PRESENCE_CADENCE`.
const PRESENCE_CADENCE: Duration = Duration::from_secs(10);
/// `watchdog/seams.rs` thresholds.
const POLL_S: u64 = 5;
const SILENCE_MS: u64 = 12_000;
const PEER_FRESH_MS: u64 = 30_000;
const COLD_HOLD_MS: u64 = 75_000;
const PEER_LOST_MS: u64 = 120_000;
/// `acquire.rs` sweep.
const DWELL_MS: u64 = 800;
const DWELL_POLLS: u64 = 4;
const SWEEP_ROUNDS: u32 = 3;
/// A terminated receive process comes back after this long.
const RX_RESTART: Duration = Duration::from_secs(2);

/// Retune ground `i`'s radio; `false` when it did not land (the radio stays
/// where it was).
async fn retune(w: &Shared, i: usize, channel: u8, why: &str) -> bool {
    let (fail, took, prev) = {
        let mut g = lock(w);
        let p = g.f.ground_retune_fail;
        let fail = g.faults_on && g.rng.chance(p);
        let took = g.rng.ms(100, 300);
        let prev = g.grounds[i].channel;
        g.grounds[i].channel = 0;
        (fail, took, prev)
    };
    sleep(Duration::from_millis(took)).await;
    let mut g = lock(w);
    let name = g.grounds[i].name;
    if fail {
        g.grounds[i].channel = prev;
        g.grounds[i].retune_failed_ms = Some(g.now_ms());
        g.log(format!(
            "{name} {why} retune {channel} FAILED, still on {prev}"
        ));
    } else {
        g.grounds[i].channel = channel;
        g.log(format!("{name} {why} retune -> {channel}"));
    }
    !fail
}

/// The control-plane receiver: follow announces, record beacons.
pub async fn receiver(w: Shared, i: usize, mut rx: mpsc::UnboundedReceiver<Vec<u8>>) {
    let key = key();
    while let Some(pkt) = rx.recv().await {
        match pkt.len() {
            51 => {
                let Some((channel, _trigger)) = parse_hop_announce(&pkt, &key) else {
                    continue;
                };
                let delay = hop_announce_delay_ms(&pkt).unwrap_or(0);
                let prev = {
                    let mut g = lock(&w);
                    if let Some(reason) = refusal(&g, i, channel) {
                        let name = g.grounds[i].name;
                        g.log(format!("{name} refuses hop -> {channel}: {reason}"));
                        continue;
                    }
                    // The ack commits both sides, so it goes only after the
                    // checks; it is repeated for every announce of the hop.
                    transmit(&w, &mut g, Node::Ground(i), &pkt);
                    if let Some(r) = g.record_for(channel) {
                        g.hops[r].acked_by.insert(i);
                    }
                    if g.grounds[i].pending == Some(channel) {
                        continue;
                    }
                    g.grounds[i].pending = Some(channel);
                    let here = g.grounds[i].channel;
                    let prev = if here != 0 {
                        Some(here)
                    } else {
                        g.grounds[i].peer_channel
                    };
                    g.grounds[i].peer_channel = Some(channel);
                    g.stats.follows += 1;
                    let name = g.grounds[i].name;
                    g.log(format!(
                        "{name} acks hop -> {channel}, follows in {} ms (prev {prev:?})",
                        delay.min(MAX_FOLLOW_WAIT_MS)
                    ));
                    prev
                };
                tokio::spawn(follow(w.clone(), i, channel, delay, prev));
            }
            68 => {
                let Some(p) = parse_presence_beacon(&pkt, &key) else {
                    continue;
                };
                if p.role != "drone" {
                    continue;
                }
                let mut g = lock(&w);
                let now = g.now_ms();
                let r = &mut g.grounds[i];
                r.peer_channel = Some(p.channel);
                r.peer_seen_ms = Some(now);
            }
            _ => {}
        }
    }
}

/// `hop_follow_refusal` for a one-slot fleet with a resolved interface.
fn refusal(g: &World, i: usize, channel: u8) -> Option<&'static str> {
    let permitted = &g.grounds[i].permitted;
    (!permitted.is_empty() && !permitted.contains(&channel)).then_some("channel_not_permitted")
}

/// `HopFollower::follow`: retune after the countdown, then verify that valid
/// packets arrive on the new channel, else go back to the one left.
async fn follow(w: Shared, i: usize, channel: u8, delay_ms: u64, prev: Option<u8>) {
    sleep(Duration::from_millis(delay_ms.min(MAX_FOLLOW_WAIT_MS))).await;
    if !retune(&w, i, channel, "follow").await {
        revert(&w, i, prev).await;
    } else {
        let before = lock(&w).grounds[i].valid;
        sleep(Duration::from_millis(FOLLOW_VERIFY_MS)).await;
        let after = lock(&w).grounds[i].valid;
        if after <= before {
            {
                let mut g = lock(&w);
                g.stats.reverts += 1;
                let name = g.grounds[i].name;
                g.log(format!("{name} follow -> {channel}: no traffic, reverting"));
            }
            revert(&w, i, prev).await;
        }
    }
    let mut g = lock(&w);
    if g.grounds[i].pending == Some(channel) {
        g.grounds[i].pending = None;
    }
}

async fn revert(w: &Shared, i: usize, prev: Option<u8>) {
    let Some(prev) = prev else {
        return;
    };
    if retune(w, i, prev, "revert").await {
        let mut g = lock(w);
        g.grounds[i].peer_channel = Some(prev);
    }
}

/// The ground's presence beacon, every 10 s on the channel it is on.
pub async fn beacons(w: Shared, i: usize) {
    let key = key();
    let mut tick = tokio::time::interval(PRESENCE_CADENCE);
    loop {
        tick.tick().await;
        let mut g = lock(&w);
        let name = g.grounds[i].name;
        let ch = g.grounds[i].channel;
        let pkt = build_presence_beacon(name, false, ch, -50, 0, &key);
        transmit(&w, &mut g, Node::Ground(i), &pkt);
    }
}

/// The sweep order: the configured band first, then every other standard
/// channel, filtered to the permitted set (`candidate_channels`).
fn candidates(g: &World, i: usize) -> Vec<u8> {
    let mut out = vec![149, 153, 157, 161, 165];
    for (c, _) in standard_channels() {
        if !out.contains(&c) {
            out.push(c);
        }
    }
    let permitted = &g.grounds[i].permitted;
    let filtered: Vec<u8> = out
        .iter()
        .copied()
        .filter(|c| permitted.contains(c))
        .collect();
    if filtered.is_empty() {
        out
    } else {
        filtered
    }
}

/// `try_channel`: retune and dwell for a valid decode.
async fn try_channel(w: &Shared, i: usize, channel: u8) -> bool {
    let baseline = lock(w).grounds[i].valid;
    if !retune(w, i, channel, "sweep").await {
        return false;
    }
    for _ in 0..DWELL_POLLS {
        sleep(Duration::from_millis(DWELL_MS / DWELL_POLLS)).await;
        if lock(w).grounds[i].valid > baseline {
            return true;
        }
    }
    false
}

/// `ValidPacketWatchdog::run`, restarted after each receive-process restart.
pub async fn watchdog(w: Shared, i: usize) {
    let mut ever_linked = false;
    let mut cold_sweep_done = false;
    let mut wd_channel = HOME;
    loop {
        let (mut last_change, mut last_count, cold_start) = {
            let g = lock(&w);
            (g.now_ms(), g.grounds[i].valid, g.now_ms())
        };
        if !ever_linked {
            cold_sweep_done = false;
        }
        let terminated = loop {
            sleep(Duration::from_secs(POLL_S)).await;
            let (now, valid, age, announced) = {
                let g = lock(&w);
                let r = &g.grounds[i];
                let now = g.now_ms();
                (
                    now,
                    r.valid,
                    r.peer_seen_ms.map(|t| now - t),
                    r.peer_channel,
                )
            };
            if valid > last_count {
                last_change = now;
                ever_linked = true;
                cold_sweep_done = false;
            }
            last_count = valid;
            if now - last_change < SILENCE_MS {
                continue;
            }
            if age.is_some_and(|a| a <= PEER_FRESH_MS) {
                ever_linked = true;
                cold_sweep_done = false;
                continue;
            }
            if ever_linked && age.is_some_and(|a| a <= PEER_LOST_MS) {
                continue;
            }
            if !ever_linked {
                if now - cold_start < COLD_HOLD_MS || cold_sweep_done {
                    continue;
                }
                cold_sweep_done = true;
            }
            // Genuine loss (or the one cold self-heal): beacon-guided lock,
            // then the band sweep, then home and a receive restart.
            {
                let mut g = lock(&w);
                g.stats.sweeps += 1;
                let name = g.grounds[i].name;
                g.log(format!(
                    "{name} watchdog: silent, sweeping (announced {announced:?})"
                ));
            }
            let mut locked = None;
            if let Some(ann) = announced {
                if ann != wd_channel && try_channel(&w, i, ann).await {
                    locked = Some(ann);
                }
            }
            if locked.is_none() {
                let order = candidates(&lock(&w), i);
                'sweep: for _ in 0..SWEEP_ROUNDS {
                    for &c in &order {
                        if try_channel(&w, i, c).await {
                            locked = Some(c);
                            break 'sweep;
                        }
                    }
                }
            }
            if let Some(c) = locked {
                wd_channel = c;
                last_change = lock(&w).now_ms();
                let mut g = lock(&w);
                let name = g.grounds[i].name;
                g.log(format!("{name} watchdog locked on {c}"));
                continue;
            }
            try_channel(&w, i, HOME).await;
            wd_channel = HOME;
            if !ever_linked {
                last_change = lock(&w).now_ms();
                continue;
            }
            break true;
        };
        if terminated {
            {
                let mut g = lock(&w);
                let name = g.grounds[i].name;
                g.log(format!(
                    "{name} watchdog: reacquire failed, receive restart"
                ));
            }
            sleep(RX_RESTART).await;
        }
    }
}

/// The valid-decode counters: every 100 ms each ground tuned to the live
/// drone's channel decodes its video, less the loss.
pub async fn traffic(w: Shared) {
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    loop {
        tick.tick().await;
        let mut g = lock(&w);
        let ch = g.drone.channel;
        if !g.drone.alive || ch == 0 || g.outage {
            continue;
        }
        let loss = if g.faults_on { g.f.loss } else { 0.01 };
        for i in 0..g.grounds.len() {
            if g.grounds[i].channel == ch && !g.rng.chance(loss) {
                g.grounds[i].valid += 1;
            }
        }
    }
}

/// Start ground `i`: its receiver, beacon and watchdog.
pub fn spawn(
    w: &Shared,
    i: usize,
    rx: mpsc::UnboundedReceiver<Vec<u8>>,
) -> Vec<tokio::task::JoinHandle<()>> {
    vec![
        tokio::spawn(receiver(w.clone(), i, rx)),
        tokio::spawn(beacons(w.clone(), i)),
        tokio::spawn(watchdog(w.clone(), i)),
    ]
}
