//! One run: a seeded world, the drone's real hop supervisor and the ground
//! stations on a paused clock in one thread, the environment's interference,
//! blackouts and bind windows, the operator, and the monitor. The faults run
//! for [`FAULTS`], then a quiet tail of [`TAIL`] in which the pair must settle.

use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tokio::time::{sleep, Duration};

use crate::drone::{self, Drone};
use crate::world::{lock, Faults, Shared, Stats, Trace, World, BAND};
use crate::{check, ground};

/// How long the faults run.
const FAULTS: Duration = Duration::from_secs(720);
/// The quiet tail: low loss, no interference, no faults. Longer than the
/// ground's 120 s presence-loss window plus a sweep.
const TAIL: Duration = Duration::from_secs(240);

/// A finished run.
pub struct Run {
    pub trace: Trace,
    pub violations: Vec<String>,
    pub stats: Stats,
    pub sim_s: f64,
}

/// Run `faults` on `seed`.
pub fn simulate(seed: u64, faults: Faults) -> Run {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .expect("a paused runtime");
    rt.block_on(async move {
        let mut inboxes = Vec::new();
        let mut receivers = Vec::new();
        for _ in 0..faults.grounds {
            let (tx, rx) = mpsc::unbounded_channel();
            inboxes.push(tx);
            receivers.push(rx);
        }
        let periodic = faults.periodic;
        let w: Shared = Arc::new(Mutex::new(World::new(seed, faults, inboxes)));
        lock(&w).log(format!("seed {seed}"));
        let d = Arc::new(Drone::new(periodic));
        let mut tasks = Vec::new();
        for (i, rx) in receivers.into_iter().enumerate() {
            tasks.extend(ground::spawn(&w, i, rx));
        }
        tasks.push(tokio::spawn(ground::traffic(w.clone())));
        tasks.push(tokio::spawn(drone::service(w.clone(), d.clone())));
        tasks.push(tokio::spawn(drone::crasher(w.clone())));
        tasks.push(tokio::spawn(drone::link_feed(w.clone(), d.clone())));
        tasks.push(tokio::spawn(check::monitor(w.clone(), d.clone())));
        tasks.push(tokio::spawn(check::hop_checks(w.clone())));
        tasks.push(tokio::spawn(check::operator(w.clone())));
        tasks.push(tokio::spawn(interference(w.clone())));
        tasks.push(tokio::spawn(outages(w.clone())));
        tasks.push(tokio::spawn(binds(w.clone())));
        tasks.push(tokio::spawn(snapshots(w.clone())));

        sleep(FAULTS).await;
        {
            let mut g = lock(&w);
            g.faults_on = false;
            g.bad = None;
            if g.outage {
                g.outage = false;
                g.disrupt();
            }
            g.bind_open = false;
            g.log("quiet tail");
        }
        sleep(TAIL).await;
        check::finish(&w, &d).await;
        for t in &tasks {
            t.abort();
        }
        let mut g = lock(&w);
        let sim_s = g.now_ms() as f64 / 1000.0;
        let stats = g.stats;
        g.log(format!("stats {stats:?}"));
        Run {
            trace: std::mem::take(&mut g.trace),
            violations: std::mem::take(&mut g.violations),
            stats,
            sim_s,
        }
    })
}

/// Interference on one channel at a time, mostly the drone's, which drives the
/// reactive hop (the uplink loss there crosses the threshold) and the scan
/// ranking.
async fn interference(w: Shared) {
    loop {
        let gap = {
            let mut g = lock(&w);
            let every = g.f.interference_every_s;
            g.rng.secs(every)
        };
        sleep(Duration::from_millis(gap)).await;
        let dur = {
            let mut g = lock(&w);
            if !g.faults_on {
                return;
            }
            let ch = if g.drone.channel != 0 && g.rng.chance(0.7) {
                g.drone.channel
            } else {
                g.rng.pick(&BAND)
            };
            g.bad = Some(ch);
            g.log(format!("interference on {ch}"));
            g.rng.ms(30_000, 60_000)
        };
        sleep(Duration::from_millis(dur)).await;
        let mut g = lock(&w);
        g.bad = None;
    }
}

/// RF blackouts: nothing reaches anyone.
async fn outages(w: Shared) {
    let Some((every, len)) = lock(&w).f.outage else {
        return;
    };
    loop {
        let gap = lock(&w).rng.secs(every);
        sleep(Duration::from_millis(gap)).await;
        let dur = {
            let mut g = lock(&w);
            if !g.faults_on {
                return;
            }
            g.outage = true;
            g.stats.outages += 1;
            g.disrupt();
            let dur = g.rng.secs(len);
            g.log(format!("outage for {} ms", dur));
            dur
        };
        sleep(Duration::from_millis(dur)).await;
        let mut g = lock(&w);
        if g.outage {
            g.outage = false;
            g.disrupt();
            g.log("outage over");
        }
    }
}

/// Bind windows: the bind sentinel is set while one runs.
async fn binds(w: Shared) {
    let Some((every, len)) = lock(&w).f.bind else {
        return;
    };
    loop {
        let gap = lock(&w).rng.secs(every);
        sleep(Duration::from_millis(gap)).await;
        let dur = {
            let mut g = lock(&w);
            if !g.faults_on {
                return;
            }
            g.bind_open = true;
            g.stats.binds += 1;
            let dur = g.rng.secs(len);
            g.log(format!("bind window for {} ms", dur));
            dur
        };
        sleep(Duration::from_millis(dur)).await;
        let mut g = lock(&w);
        if g.bind_open {
            g.bind_open = false;
            g.log("bind window over");
        }
    }
}

/// A state line every 10 s.
async fn snapshots(w: Shared) {
    let mut tick = tokio::time::interval(Duration::from_secs(10));
    loop {
        tick.tick().await;
        let mut g = lock(&w);
        let grounds: Vec<String> = g
            .grounds
            .iter()
            .map(|r| format!("{}={} valid {}", r.name, r.channel, r.valid))
            .collect();
        let line = format!(
            "state drone={} alive={} gen={} {}",
            g.drone.channel,
            g.drone.alive,
            g.drone.generation,
            grounds.join(" ")
        );
        g.log(line);
    }
}
