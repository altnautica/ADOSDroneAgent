use super::*;

fn at(base: Instant, ms: u64) -> Instant {
    base + Duration::from_millis(ms)
}

#[test]
fn the_first_arrival_contributes_no_gap() {
    // Otherwise the age of the process is reported as a period.
    let mut c = InboundCadence::new(Duration::from_millis(50));
    let t0 = Instant::now();
    c.record(t0);
    let s = c.snapshot();
    assert_eq!(s.arrivals, 1);
    assert_eq!(s.samples, 0);
    assert!(s.achieved_hz.is_none());
    assert!(s.p99.is_none());
}

#[test]
fn an_even_stream_reports_its_real_rate() {
    let mut c = InboundCadence::new(Duration::from_millis(50));
    let t0 = Instant::now();
    // 20 ms apart is 50 Hz.
    for i in 0..=10 {
        c.record(at(t0, i * 20));
    }
    let s = c.snapshot();
    assert_eq!(s.samples, 10);
    let hz = s.achieved_hz.expect("rate");
    assert!((hz - 50.0).abs() < 0.5, "got {hz}");
    assert_eq!(s.p50, Some(Duration::from_millis(20)));
    assert_eq!(s.p99, Some(Duration::from_millis(20)));
}

#[test]
fn one_stall_moves_the_tail_without_moving_the_median() {
    // The whole reason the tail is measured separately. A stream that is
    // punctual except for one long gap has an unchanged median and a
    // materially worse p99, and it is the p99 a control loop lives or dies
    // on.
    let mut c = InboundCadence::new(Duration::from_millis(50));
    let t0 = Instant::now();
    let mut when = 0u64;
    for _ in 0..99 {
        c.record(at(t0, when));
        when += 10;
    }
    when += 300; // one long stall
    c.record(at(t0, when));

    let s = c.snapshot();
    assert_eq!(
        s.p50,
        Some(Duration::from_millis(10)),
        "median is untouched"
    );
    assert_eq!(
        s.worst,
        Some(Duration::from_millis(310)),
        "and the stall is reported at its real size, not smoothed away"
    );
    assert!(
        s.p99.expect("p99") > s.p50.expect("p50"),
        "the tail sees what the median does not"
    );
}

#[test]
fn a_late_arrival_is_counted_against_the_deadline() {
    let mut c = InboundCadence::new(Duration::from_millis(25));
    let t0 = Instant::now();
    c.record(at(t0, 0));
    c.record(at(t0, 20)); // inside
    c.record(at(t0, 60)); // 40 ms gap, late
    c.record(at(t0, 80)); // inside
    assert_eq!(c.snapshot().missed_deadline, 1);
}

#[test]
fn a_deadline_miss_count_survives_a_stream_reset() {
    // The counter answers "has this link ever been late", which a
    // reconnection does not un-answer.
    let mut c = InboundCadence::new(Duration::from_millis(25));
    let t0 = Instant::now();
    c.record(at(t0, 0));
    c.record(at(t0, 100));
    assert_eq!(c.snapshot().missed_deadline, 1);
    c.reset_stream();
    assert_eq!(c.snapshot().missed_deadline, 1);
}

#[test]
fn a_reset_stops_an_outage_being_reported_as_a_slow_controller() {
    // Without this the gap spanning a dropped link lands in a distribution
    // that otherwise describes milliseconds, and one reconnect would make
    // the tail meaningless for as long as the window retained it.
    let mut c = InboundCadence::new(Duration::from_millis(50));
    let t0 = Instant::now();
    c.record(at(t0, 0));
    c.record(at(t0, 20));
    c.reset_stream();
    c.record(at(t0, 30_000)); // link came back half a minute later
    c.record(at(t0, 30_020));

    let s = c.snapshot();
    assert_eq!(
        s.worst,
        Some(Duration::from_millis(20)),
        "the outage is not in the distribution"
    );
    assert_eq!(s.arrivals, 4, "but every arrival is still counted");
}

#[test]
fn the_window_is_bounded_so_a_long_run_costs_no_more_than_a_short_one() {
    let mut c = InboundCadence::new(Duration::from_millis(50));
    let t0 = Instant::now();
    for i in 0..(CADENCE_WINDOW as u64 * 3) {
        c.record(at(t0, i * 5));
    }
    let s = c.snapshot();
    assert_eq!(s.samples, CADENCE_WINDOW);
    assert_eq!(s.arrivals, CADENCE_WINDOW as u64 * 3);
}

#[test]
fn a_backward_clock_step_yields_no_sample_rather_than_a_wrong_one() {
    let mut c = InboundCadence::new(Duration::from_millis(50));
    let t0 = Instant::now() + Duration::from_secs(10);
    c.record(t0);
    c.record(t0 - Duration::from_secs(1));
    assert_eq!(c.snapshot().samples, 0);
}

#[test]
fn percentiles_select_a_real_sample_never_an_interpolation() {
    // A measured figure that no message actually produced is a fabricated
    // number, however reasonable it looks.
    let sorted: Vec<Duration> = (1..=100).map(Duration::from_millis).collect();
    for q in [0.50, 0.95, 0.99] {
        let p = percentile(&sorted, q);
        assert!(sorted.contains(&p), "{q} produced {p:?}");
    }
    assert_eq!(percentile(&sorted, 0.99), Duration::from_millis(99));
}
