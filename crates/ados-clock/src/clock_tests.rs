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
        sim_time: source == SyncSource::Sim,
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
        "version", "timebase", "source", "locked", "sigma_ns", "t_ns", "utc_ns", "sources",
    ];
    let mut got = keys.clone();
    got.sort_unstable();
    want.sort_unstable();
    assert_eq!(got, want);
    assert_eq!(v["version"], 1);
    assert_eq!(v["timebase"], "hub_freerun");
    assert_eq!(v["source"], "hub");
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
            "sim_time",
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
