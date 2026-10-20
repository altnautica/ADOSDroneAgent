//! Simulator time: the video SEI envelope, its latency floor, and the
//! simulated hub's exact path.

use super::*;
use crate::fit::UNKNOWN_FLOOR_BOUND_NS;

const MS: u64 = 1_000_000;
const S: u64 = 1_000_000_000;
/// A boot-relative host time far from zero.
const T0: u64 = 500 * S;

fn sync(source: SyncSource, host_ns: u64, source_ns: i64, sim_time: bool) -> SyncObs {
    SyncObs {
        source,
        host_ns,
        source_ns,
        rtt_ns: 0,
        sim_time,
    }
}

/// Deterministic uniform `(0, 1)` draws.
struct Lcg(u64);

impl Lcg {
    fn unit(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }
}

/// Simulator time at host time `h` of a run whose clock read 0 at host `h0`
/// and runs 40 ppm fast.
fn sim_at(h0: u64, h: u64) -> i64 {
    ((h as f64 - h0 as f64) * (1.0 + 40e-6)).round() as i64
}

/// How a rendered run reaches the host.
#[derive(Clone, Copy)]
struct Video {
    cams: u64,
    fps: u64,
    /// The pipeline's minimum latency.
    floor_ns: u64,
}

const ONE_CAM: Video = Video {
    cams: 1,
    fps: 20,
    floor_ns: 0,
};

/// SEI pairs of the run of [`sim_at`]`(h0, _)`, frames rendered from host
/// `from` until `until`: each reaches the host `floor_ns` plus an exponential
/// latency (mean 60 ms) after render, every 25th frame of a camera 200 ms
/// later still; in arrival order.
fn sei_pairs(rng: &mut Lcg, h0: u64, from: u64, until: u64, v: Video) -> Vec<(u64, i64)> {
    let mut pairs = Vec::new();
    let (mut emit, mut frame) = (from, 0u64);
    while emit < until {
        for _ in 0..v.cams {
            let mut latency = -60.0 * MS as f64 * rng.unit().ln();
            if frame % 25 == 24 {
                latency += 200.0 * MS as f64;
            }
            pairs.push((emit + v.floor_ns + latency as u64, sim_at(h0, emit)));
        }
        emit += S / v.fps;
        frame += 1;
    }
    pairs.sort_unstable();
    pairs
}

/// Feed SEI `pairs`: the outcomes, the first host time the sim source was
/// locked, and the worst session-time error against `want` while locked.
/// Once locked it must stay locked.
fn feed_sei(
    c: &mut ClockState,
    pairs: &[(u64, i64)],
    want: impl Fn(u64) -> i64,
) -> (Vec<SyncOutcome>, Option<u64>, u64) {
    let (mut outcomes, mut locked_at, mut worst) = (Vec::new(), None, 0);
    for &(host, sim) in pairs {
        outcomes.push(c.on_sync(&sync(SyncSource::Sim, host, sim, true)));
        if c.locked(SyncSource::Sim, host) {
            locked_at.get_or_insert(host);
            let got = c.sample(host).t_ns as i64;
            worst = worst.max(got.abs_diff(want(host)));
        } else {
            assert!(
                locked_at.is_none(),
                "lost lock at {host}: {:?}",
                c.report(host).sources[3]
            );
        }
    }
    (outcomes, locked_at, worst)
}

#[test]
fn arrival_jitter_and_spikes_lock_the_sim_envelope() {
    for seed in 1..=6 {
        let mut c = ClockState::new();
        let pairs = sei_pairs(&mut Lcg(seed), T0, T0, T0 + 90 * S, ONE_CAM);
        let (outcomes, locked_at, worst) = feed_sei(&mut c, &pairs, |h| sim_at(T0, h));
        assert!(
            !outcomes.contains(&SyncOutcome::Reset),
            "seed {seed}: a late pair reset the fit"
        );
        let locked_at = locked_at.unwrap_or_else(|| panic!("seed {seed}: never locked"));
        assert!(
            (T0 + 40 * S..T0 + 50 * S).contains(&locked_at),
            "seed {seed}: locked at {locked_at}"
        );
        assert!(worst < MS, "seed {seed}: worst offset error {worst} ns");
        let now = pairs.last().unwrap().0;
        let r = c.report(now);
        assert_eq!((r.timebase, r.source), (Timebase::Sim, Some("sim")));
        let sim = &r.sources[3];
        assert!(sim.locked && sim.sim_time);
        // No floor configured: the published sigma carries its bound.
        let bound = UNKNOWN_FLOOR_BOUND_NS as i64;
        assert!(
            (bound..bound + LOCK_MAX_SIGMA_NS).contains(&sim.sigma_ns),
            "seed {seed}: sigma {}",
            sim.sigma_ns
        );
        assert_eq!(r.sigma_ns, sim.sigma_ns);
        assert!(
            (sim.skew_ppm - 40.0).abs() < 20.0,
            "seed {seed}: skew {}",
            sim.skew_ppm
        );
    }
}

#[test]
fn four_cameras_at_thirty_fps_still_lock() {
    let video = Video {
        cams: 4,
        fps: 30,
        floor_ns: 0,
    };
    for seed in 1..=3 {
        let mut c = ClockState::new();
        let pairs = sei_pairs(&mut Lcg(seed), T0, T0, T0 + 90 * S, video);
        assert!(pairs.len() > 4096, "{} pairs", pairs.len());
        let (outcomes, locked_at, worst) = feed_sei(&mut c, &pairs, |h| sim_at(T0, h));
        assert!(!outcomes.contains(&SyncOutcome::Reset), "seed {seed}");
        let locked_at = locked_at.unwrap_or_else(|| panic!("seed {seed}: never locked"));
        assert!(
            (T0 + 40 * S..T0 + 50 * S).contains(&locked_at),
            "seed {seed}: locked at {locked_at}"
        );
        assert!(worst < MS, "seed {seed}: worst offset error {worst} ns");
    }
}

#[test]
fn a_known_floor_is_added_to_the_envelope() {
    let video = Video {
        floor_ns: 25 * MS,
        ..ONE_CAM
    };
    let mut c = ClockState::with_sim_floor(LatencyFloor::Known(25 * MS));
    let pairs = sei_pairs(&mut Lcg(2), T0, T0, T0 + 90 * S, video);
    let (_, locked_at, worst) = feed_sei(&mut c, &pairs, |h| sim_at(T0, h));
    assert!(locked_at.is_some());
    assert!(worst < MS, "worst offset error {worst} ns");
    let r = c.report(pairs.last().unwrap().0);
    assert!(
        (0..LOCK_MAX_SIGMA_NS).contains(&r.sigma_ns),
        "sigma {}",
        r.sigma_ns
    );
}

#[test]
fn an_unknown_floor_biases_the_offset_inside_the_published_sigma() {
    let video = Video {
        floor_ns: 25 * MS,
        ..ONE_CAM
    };
    let mut c = ClockState::new();
    let pairs = sei_pairs(&mut Lcg(2), T0, T0, T0 + 90 * S, video);
    // The envelope tracks the relation shifted by the floor.
    let (_, locked_at, worst) = feed_sei(&mut c, &pairs, |h| sim_at(T0, h) - 25 * MS as i64);
    assert!(locked_at.is_some());
    assert!(
        worst < MS,
        "worst error against the shifted relation {worst} ns"
    );
    let now = pairs.last().unwrap().0;
    let s = c.sample(now);
    let error = (s.t_ns as i64).abs_diff(sim_at(T0, now));
    assert!(s.locked && s.sigma_ns >= UNKNOWN_FLOOR_BOUND_NS as i64);
    assert!(
        error <= s.sigma_ns as u64,
        "error {error} beyond sigma {}",
        s.sigma_ns
    );
}

#[test]
fn a_simulator_restart_resets_the_sim_envelope() {
    let mut c = ClockState::new();
    let mut rng = Lcg(3);
    let first_run = sei_pairs(&mut rng, T0, T0, T0 + 60 * S, ONE_CAM);
    let (outcomes, locked_at, _) = feed_sei(&mut c, &first_run, |h| sim_at(T0, h));
    assert!(!outcomes.contains(&SyncOutcome::Reset));
    assert!(locked_at.is_some());
    // A second run a second later: simulator time starts over at zero.
    let h1 = T0 + 61 * S;
    let mut second_run = sei_pairs(&mut rng, h1, h1, h1 + 60 * S, ONE_CAM);
    let first: Vec<SyncOutcome> = second_run
        .drain(..3)
        .map(|(host, sim)| c.on_sync(&sync(SyncSource::Sim, host, sim, true)))
        .collect();
    assert_eq!(
        first,
        [
            SyncOutcome::Outlier,
            SyncOutcome::Outlier,
            SyncOutcome::Reset
        ]
    );
    assert!(!c.locked(SyncSource::Sim, second_run[0].0));
    let (outcomes, locked_at, worst) = feed_sei(&mut c, &second_run, |h| sim_at(h1, h));
    assert!(!outcomes.contains(&SyncOutcome::Reset));
    let locked_at = locked_at.expect("relocked on the new run");
    assert!(locked_at >= h1 + 40 * S, "relocked at {locked_at}");
    assert!(
        worst < MS,
        "worst offset error {worst} ns after the restart"
    );
}

#[test]
fn a_lower_latency_floor_restarts_the_envelope_and_relocks() {
    let mut c = ClockState::new();
    let mut rng = Lcg(5);
    let high = Video {
        floor_ns: 30 * MS,
        ..ONE_CAM
    };
    let before = sei_pairs(&mut rng, T0, T0, T0 + 60 * S, high);
    let (outcomes, locked_at, worst) =
        feed_sei(&mut c, &before, |h| sim_at(T0, h) - 30 * MS as i64);
    assert!(!outcomes.contains(&SyncOutcome::Reset) && locked_at.is_some());
    assert!(worst < MS, "worst {worst} ns before the step");
    // A backlog drains: the same run now arrives 25 ms sooner.
    let low = Video {
        floor_ns: 5 * MS,
        ..ONE_CAM
    };
    let step = T0 + 60 * S;
    let after = sei_pairs(&mut rng, T0, step, step + 120 * S, low);
    let (mut reset_at, mut relocked_at, mut worst) = (None, None, 0);
    for &(host, sim) in &after {
        let outcome = c.on_sync(&sync(SyncSource::Sim, host, sim, true));
        if outcome == SyncOutcome::Reset {
            assert!(reset_at.is_none(), "a second reset at {host}");
            reset_at = Some(host);
        }
        if reset_at.is_some() && c.locked(SyncSource::Sim, host) {
            relocked_at.get_or_insert(host);
            let got = c.sample(host).t_ns as i64;
            worst = worst.max(got.abs_diff(sim_at(T0, host) - 5 * MS as i64));
        }
    }
    let reset_at = reset_at.expect("the step restarted the envelope");
    assert!(reset_at < step + 10 * S, "reset at {reset_at}");
    let relocked_at = relocked_at.expect("relocked on the lower floor");
    assert!(relocked_at >= reset_at + 40 * S);
    assert!(worst < MS, "worst {worst} ns after the step");
}

#[test]
fn a_simulator_hub_is_the_exact_sim_source() {
    let mut c = ClockState::new();
    // The hub's own one-per-second envelope pairs, on simulator time.
    for i in 0..6u64 {
        let h = T0 + i * S;
        c.on_sync(&sync(SyncSource::Hub, h, sim_at(T0, h), true));
    }
    // SEI pairs are arriving too, not yet locked.
    let pairs = sei_pairs(&mut Lcg(1), T0, T0, T0 + 5 * S, ONE_CAM);
    for &(host, sim) in &pairs {
        c.on_sync(&sync(SyncSource::Sim, host, sim, true));
    }
    let now = T0 + 5 * S + 500 * MS;
    let r = c.report(now);
    assert_eq!((r.timebase, r.source), (Timebase::Sim, Some("hub")));
    assert!(r.sources[1].sim_time && !r.sources[3].locked);
    assert!((0..LOCK_MAX_SIGMA_NS).contains(&r.sigma_ns));
    let s = c.sample(now);
    assert!((s.t_ns as i64).abs_diff(sim_at(T0, now)) <= 2);
}

#[test]
fn a_flight_hub_stays_free_running() {
    let mut c = ClockState::new();
    for i in 0..6u64 {
        let h = T0 + i * S;
        c.on_sync(&sync(SyncSource::Hub, h, h as i64 + 7_000, false));
    }
    let r = c.report(T0 + 5 * S);
    assert_eq!((r.timebase, r.source), (Timebase::HubFreerun, Some("hub")));
}

#[test]
fn losing_the_simulator_hub_falls_back_to_the_envelope_with_a_change_event() {
    let mut c = ClockState::new();
    // The hub publishes for 50 s, the video for 70 s.
    let mut events: Vec<(u64, i64, SyncSource)> = (0..50u64)
        .map(|i| (T0 + i * S, sim_at(T0, T0 + i * S), SyncSource::Hub))
        .collect();
    events.extend(
        sei_pairs(&mut Lcg(4), T0, T0, T0 + 70 * S, ONE_CAM)
            .into_iter()
            .map(|(h, s)| (h, s, SyncSource::Sim)),
    );
    events.sort_unstable_by_key(|e| e.0);
    let mut changes = Vec::new();
    let mut next_tick = T0;
    for (host, sim, source) in events {
        c.on_sync(&sync(source, host, sim, true));
        if host >= next_tick {
            changes.extend(c.track(host));
            next_tick += S;
        }
    }
    let tb = |c: &SourceChange| (c.from_timebase, c.from, c.to_timebase, c.to);
    assert_eq!(changes.len(), 2, "{changes:?}");
    assert_eq!(
        tb(&changes[0]),
        (Timebase::HostMonotonic, None, Timebase::Sim, Some("hub"))
    );
    let drop = changes[1];
    assert_eq!(
        tb(&drop),
        (Timebase::Sim, Some("hub"), Timebase::Sim, Some("sim"))
    );
    // The hub's last pair was at T0 + 49 s; it stops counting 3 s later.
    assert!((T0 + 52 * S..T0 + 54 * S).contains(&drop.host_ns));
    let step = drop.step_ns.expect("both lines measure the step");
    assert!(step.unsigned_abs() < MS, "step {step} ns");
    let r = c.report(T0 + 60 * S);
    assert_eq!(r.source_changed, Some(drop));
    assert_eq!(r.source, Some("sim"));
}
