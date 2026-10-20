use super::*;

const MS: u64 = 1_000_000;
const S: u64 = 1_000_000_000;
/// A boot-relative host time far from zero.
const T0: u64 = 500 * S;

fn obs(source: SyncSource, host_ns: u64, source_ns: i64) -> SyncObs {
    SyncObs {
        source,
        host_ns,
        source_ns,
        rtt_ns: 0,
    }
}

/// Feed `n` pairs 100 ms apart ending at `end`, source = host + offset.
fn feed(c: &mut ClockState, source: SyncSource, n: u64, end: u64, offset: i64) {
    for i in 0..n {
        let h = end - (n - 1 - i) * 100 * MS;
        c.on_sync(&obs(source, h, h as i64 + offset));
    }
}

#[test]
fn lock_needs_five_fresh_tight_pairs() {
    let mut c = ClockState::new();
    feed(&mut c, SyncSource::Hub, 4, T0, 12_000);
    assert!(!c.locked(SyncSource::Hub, T0), "four pairs are not enough");
    feed(&mut c, SyncSource::Hub, 1, T0 + 100 * MS, 12_000);
    assert!(c.locked(SyncSource::Hub, T0 + 100 * MS));
    // Stale: the newest pair is 3 s old.
    assert!(!c.locked(SyncSource::Hub, T0 + 100 * MS + 3 * S));
    assert!(c.locked(SyncSource::Hub, T0 + 100 * MS + 3 * S - 1));
}

#[test]
fn a_noisy_fit_is_not_locked() {
    let mut c = ClockState::new();
    // +-8 ms jitter: sigma above 5 ms, every pair within the reset threshold.
    for i in 0..10u64 {
        let h = T0 + i * 100 * MS;
        let jitter = if i % 2 == 0 {
            8 * MS as i64
        } else {
            -(8 * MS as i64)
        };
        let got = c.on_sync(&obs(SyncSource::Fc, h, h as i64 + jitter));
        assert_eq!(got, SyncOutcome::Accepted, "pair {i}");
    }
    let sigma = c.report(T0 + S).sources[0].sigma_ns;
    assert!(sigma > LOCK_MAX_SIGMA_NS, "sigma {sigma}");
    assert!(!c.locked(SyncSource::Fc, T0 + S));
}

#[test]
fn one_wild_pair_is_dropped_and_the_lock_holds() {
    let mut c = ClockState::new();
    feed(&mut c, SyncSource::Fc, 10, T0, -37 * MS as i64);
    assert!(c.locked(SyncSource::Fc, T0));
    // 40 ms off the prediction: beyond max(5 sigma, 1 ms), inside the reset
    // threshold. Dropped, counted, and the fit is untouched.
    let h = T0 + 100 * MS;
    let wild = h as i64 - 37 * MS as i64 + 40 * MS as i64;
    assert_eq!(
        c.on_sync(&obs(SyncSource::Fc, h, wild)),
        SyncOutcome::Outlier
    );
    assert_eq!(c.outliers(SyncSource::Fc), 1);
    assert!(c.locked(SyncSource::Fc, h));
    let r = c.report(h);
    assert_eq!(r.sources[0].offset_ns, -37 * MS as i64);
    assert_eq!(r.sources[0].sigma_ns, 0);
    assert_eq!(r.sources[0].last_update_ns, T0);
    // A single pair past the reset threshold is dropped too.
    let h = T0 + 200 * MS;
    assert_eq!(
        c.on_sync(&obs(SyncSource::Fc, h, 1_000)),
        SyncOutcome::Outlier
    );
    // A good pair clears the run.
    let h = T0 + 300 * MS;
    let good = h as i64 - 37 * MS as i64;
    assert_eq!(
        c.on_sync(&obs(SyncSource::Fc, h, good)),
        SyncOutcome::Accepted
    );
    assert!(c.locked(SyncSource::Fc, h));
}

#[test]
fn a_source_reboot_resets_the_fit_after_three_pairs() {
    let mut c = ClockState::new();
    feed(&mut c, SyncSource::Fc, 10, T0, -37 * MS as i64);
    // The source rebooted: its clock restarted near zero.
    for (k, want) in [
        (1u64, SyncOutcome::Outlier),
        (2, SyncOutcome::Outlier),
        (3, SyncOutcome::Reset),
    ] {
        let h = T0 + k * 100 * MS;
        assert_eq!(
            c.on_sync(&obs(SyncSource::Fc, h, (k * 100 * MS) as i64)),
            want
        );
    }
    let h = T0 + 300 * MS;
    assert!(!c.locked(SyncSource::Fc, h), "one pair after a reset");
    let r = c.report(h);
    assert_eq!(r.sources[0].offset_ns, (300 * MS) as i64 - h as i64);
    assert_eq!(r.sources[0].last_update_ns, h);
}

#[test]
fn a_fit_older_than_the_window_starts_over() {
    let mut c = ClockState::new();
    feed(&mut c, SyncSource::Hub, 6, T0, 12_000);
    // Back after two minutes with a different offset: no prediction applies.
    let h = T0 + 120 * S;
    let got = c.on_sync(&obs(SyncSource::Hub, h, h as i64 + 900 * MS as i64));
    assert_eq!(got, SyncOutcome::Accepted);
    assert_eq!(c.report(h).sources[1].offset_ns, 900 * MS as i64);
}

#[test]
fn timebase_selection_table() {
    use Timebase::*;
    for (sim, gnss, hub, want) in [
        (true, true, true, Sim),
        (true, false, false, Sim),
        (false, true, true, Gnss),
        (false, true, false, Gnss),
        (false, false, true, HubFreerun),
        (false, false, false, HostMonotonic),
    ] {
        assert_eq!(choose_timebase(sim, gnss, hub), want, "{sim} {gnss} {hub}");
    }
}

#[test]
fn host_monotonic_when_nothing_is_locked() {
    let c = ClockState::new();
    let r = c.report(T0);
    assert_eq!(r.timebase, Timebase::HostMonotonic);
    assert!(!r.locked);
    assert_eq!(r.sigma_ns, -1);
    assert_eq!(r.utc_ns, None);
    assert_eq!(r.t_ns, T0);
    let s = c.sample(T0);
    assert_eq!(
        (s.t_ns, s.host_mono_ns, s.locked, s.sigma_ns),
        (T0, T0, false, -1)
    );
    assert_eq!(s.utc_ns, None);
}

#[test]
fn gnss_gives_utc_and_the_session_time() {
    let mut c = ClockState::new();
    let utc_offset: i64 = 1_760_860_000 * S as i64;
    feed(&mut c, SyncSource::GnssPps, 6, T0, utc_offset);
    feed(&mut c, SyncSource::Hub, 6, T0, 5_000);
    let now = T0 + 500 * MS;
    let r = c.report(now);
    assert_eq!(r.timebase, Timebase::Gnss);
    assert!(r.locked);
    // The fit is centred on an integer offset, so ~1.8e18 ns keeps ns precision.
    assert_eq!(r.sigma_ns, 0);
    let want = now as i64 + utc_offset;
    assert!(r.utc_ns.unwrap().abs_diff(want) <= 2);
    let s = c.sample(now);
    assert!((s.t_ns as i64).abs_diff(want) <= 2);
    assert!(s.utc_ns.unwrap().abs_diff(want) <= 2);
}

#[test]
fn hub_freerun_session_time_follows_the_hub_fit() {
    let mut c = ClockState::new();
    feed(&mut c, SyncSource::Hub, 6, T0, -7_000);
    let s = c.sample(T0);
    assert!(s.locked);
    assert_eq!(s.t_ns, T0 - 7_000);
    assert_eq!(s.utc_ns, None);
    assert_eq!(c.report(T0).timebase, Timebase::HubFreerun);
}

#[test]
fn clock_document_shape() {
    let mut c = ClockState::new();
    feed(&mut c, SyncSource::Hub, 6, T0, 12_000);
    let v = serde_json::to_value(c.report(T0)).unwrap();
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    let mut want = vec![
        "version", "timebase", "locked", "sigma_ns", "t_ns", "utc_ns", "sources",
    ];
    let mut got = keys.clone();
    got.sort_unstable();
    want.sort_unstable();
    assert_eq!(got, want);
    assert_eq!(v["version"], 1);
    assert_eq!(v["timebase"], "hub_freerun");
    assert!(v["utc_ns"].is_null());
    let sources = v["sources"].as_array().unwrap();
    let ids: Vec<&str> = sources.iter().map(|s| s["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["fc", "hub", "gnss", "sim"]);
    let kinds: Vec<&str> = sources
        .iter()
        .map(|s| s["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["fc", "hub", "gnss_pps", "sim"]);
    let mut src_keys: Vec<&str> = sources[1]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    src_keys.sort_unstable();
    assert_eq!(
        src_keys,
        [
            "id",
            "kind",
            "last_update_ns",
            "locked",
            "offset_ns",
            "sigma_ns",
            "skew_ppm"
        ]
    );
    assert_eq!(sources[1]["offset_ns"], 12_000);
    assert_eq!(sources[1]["locked"], true);
    assert_eq!(sources[0]["offset_ns"], 0);
    assert_eq!(sources[0]["sigma_ns"], -1);
    assert_eq!(sources[0]["last_update_ns"], 0);
    assert_eq!(sources[0]["skew_ppm"], 0.0);
}

/// Deterministic uniform `(0, 1)` draws for the jitter tests.
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

/// Frames rendered at 20 Hz from host `h0` until `until` by the run of
/// [`sim_at`]`(h0, _)`, each reaching the host after an exponential latency
/// (mean 60 ms) and every 25th 200 ms later still; in arrival order.
fn sim_pairs(rng: &mut Lcg, h0: u64, until: u64) -> Vec<(u64, i64)> {
    let mut pairs = Vec::new();
    let (mut emit, mut frame) = (h0, 0u64);
    while emit < until {
        let mut latency = -60.0 * MS as f64 * rng.unit().ln();
        if frame % 25 == 24 {
            latency += 200.0 * MS as f64;
        }
        pairs.push((emit + latency as u64, sim_at(h0, emit)));
        emit += 50 * MS;
        frame += 1;
    }
    pairs.sort_unstable();
    pairs
}

/// Feed `pairs` of the run started at `h0`: the outcomes, the first host time
/// the sim source was locked, and the worst session-time error while locked.
/// Once locked it must stay locked.
fn feed_sim(
    c: &mut ClockState,
    pairs: &[(u64, i64)],
    h0: u64,
) -> (Vec<SyncOutcome>, Option<u64>, u64) {
    let (mut outcomes, mut locked_at, mut worst) = (Vec::new(), None, 0);
    for &(host, sim) in pairs {
        outcomes.push(c.on_sync(&obs(SyncSource::Sim, host, sim)));
        if c.locked(SyncSource::Sim, host) {
            locked_at.get_or_insert(host);
            let got = c.sample(host).t_ns as i64;
            worst = worst.max(got.abs_diff(sim_at(h0, host)));
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
        let pairs = sim_pairs(&mut Lcg(seed), T0, T0 + 90 * S);
        let (outcomes, locked_at, worst) = feed_sim(&mut c, &pairs, T0);
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
        assert_eq!(r.timebase, Timebase::Sim);
        let sim = &r.sources[3];
        assert!(sim.locked);
        assert!(
            (0..LOCK_MAX_SIGMA_NS).contains(&sim.sigma_ns),
            "seed {seed}: sigma {}",
            sim.sigma_ns
        );
        assert!(
            (sim.skew_ppm - 40.0).abs() < 20.0,
            "seed {seed}: skew {}",
            sim.skew_ppm
        );
    }
}

#[test]
fn a_simulator_restart_resets_the_sim_envelope() {
    let mut c = ClockState::new();
    let mut rng = Lcg(3);
    let first_run = sim_pairs(&mut rng, T0, T0 + 60 * S);
    let (outcomes, locked_at, _) = feed_sim(&mut c, &first_run, T0);
    assert!(!outcomes.contains(&SyncOutcome::Reset));
    assert!(locked_at.is_some());
    // A second run a second later: simulator time starts over at zero.
    let h1 = T0 + 61 * S;
    let mut second_run = sim_pairs(&mut rng, h1, h1 + 60 * S);
    let first: Vec<SyncOutcome> = second_run
        .drain(..3)
        .map(|(host, sim)| c.on_sync(&obs(SyncSource::Sim, host, sim)))
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
    let (outcomes, locked_at, worst) = feed_sim(&mut c, &second_run, h1);
    assert!(!outcomes.contains(&SyncOutcome::Reset));
    let locked_at = locked_at.expect("relocked on the new run");
    assert!(locked_at >= h1 + 40 * S, "relocked at {locked_at}");
    assert!(
        worst < MS,
        "worst offset error {worst} ns after the restart"
    );
}
