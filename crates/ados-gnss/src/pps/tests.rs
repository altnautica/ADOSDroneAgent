use super::*;

const S: u64 = 1_000_000_000;

#[test]
fn edge_converts_realtime_to_host_monotonic() {
    let e = Edge {
        realtime_ns: 1_760_000_000_000_000_000,
        rt_minus_mono_ns: 1_759_999_000_000_000_000,
    };
    assert_eq!(e.host_ns(), 1_000_000_000_000);
}

#[test]
fn tim_tp_names_the_next_pulse_once() {
    let mut p = Pairer::default();
    let utc = 1_760_955_438 * S as i64;
    // Announced 0.9 s before the pulse.
    p.on_tim_tp(100 * S, utc);
    assert_eq!(p.on_pulse(100 * S + 900_000_000), Some(utc));
    assert!(p.locked(101 * S));
    // The same announcement is not reused for a later pulse inside its
    // window, and without a NAV-PVT time there is no UTC for it.
    assert_eq!(p.on_pulse(101 * S + 400_000_000), None);
    assert_eq!(p.on_pulse(101 * S + 900_000_000), None);
    // Lock holds for 2 s after the last paired pulse, then drops.
    assert!(p.locked(100 * S + 900_000_000 + LOCK_WINDOW_NS));
    assert!(!p.locked(100 * S + 900_000_000 + LOCK_WINDOW_NS + 1));
}

#[test]
fn stale_or_future_tim_tp_is_ignored() {
    let mut p = Pairer::default();
    p.on_tim_tp(100 * S, 7 * S as i64);
    assert_eq!(p.on_pulse(102 * S), None);
    let mut p = Pairer::default();
    p.on_tim_tp(100 * S, 7 * S as i64);
    assert_eq!(p.on_pulse(99 * S), None);
    assert!(!p.locked(99 * S));
}

#[test]
fn nav_pvt_time_rounds_to_the_pulse_second() {
    let mut p = Pairer::default();
    // Epoch 12:00:00.000 UTC reported, arriving 120 ms after the epoch.
    let epoch = 1_792_404_900 * S as i64;
    p.on_pvt_time(50 * S + 120_000_000, epoch);
    // The pulse at the next second boundary, 1 s after the epoch.
    let pulse = 51 * S + 3_000;
    assert_eq!(p.on_pulse(pulse), Some(epoch + S as i64));
    // A PVT time older than the window does not pair.
    let mut p = Pairer::default();
    p.on_pvt_time(10 * S, epoch);
    assert_eq!(p.on_pulse(13 * S), None);
}

#[test]
fn tim_tp_wins_over_nav_pvt() {
    let mut p = Pairer::default();
    p.on_pvt_time(10 * S, 500 * S as i64);
    p.on_tim_tp(10 * S + 100, 900 * S as i64);
    assert_eq!(p.on_pulse(11 * S), Some(900 * S as i64));
}

/// A scripted device for the reader loop.
struct Script(Vec<Option<Edge>>);

impl PpsSource for Script {
    fn fetch(&mut self, _t: Duration) -> io::Result<Option<Edge>> {
        Ok(self.0.pop().flatten())
    }
}

#[test]
fn source_trait_yields_edges() {
    let mut s: Box<dyn PpsSource> = Box::new(Script(vec![
        Some(Edge {
            realtime_ns: 5,
            rt_minus_mono_ns: 2,
        }),
        None,
    ]));
    assert_eq!(s.fetch(Duration::from_secs(1)).unwrap(), None);
    assert_eq!(
        s.fetch(Duration::from_secs(1)).unwrap().unwrap().host_ns(),
        3
    );
}
