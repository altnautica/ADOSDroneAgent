# ados-clock-fit

[![crates.io](https://img.shields.io/crates/v/ados-clock-fit.svg)](https://crates.io/crates/ados-clock-fit)
[![docs.rs](https://img.shields.io/docsrs/ados-clock-fit)](https://docs.rs/ados-clock-fit)
[![license](https://img.shields.io/crates/l/ados-clock-fit.svg)](#license)
[![MSRV](https://img.shields.io/badge/MSRV-1.93-blue.svg)](https://github.com/altnautica/ADOSDroneAgent/blob/main/crates/Cargo.toml)

**Map remote clocks onto a host monotonic timebase: windowed least-squares and lower-envelope fits, NTP-style TIMESYNC estimation and a chrony SOCK refclock sink.**

A flight controller, a sensor board or a GNSS receiver each stamp data with their own clock, which starts at a different moment and drifts at its own rate. To fuse that data you need every stamp on one timebase. This crate fits the relation between a remote clock and the host monotonic clock from pairs of readings, then converts timestamps in either direction. Reach for it when you log or fuse data from several devices on one computer.

```rust
use ados_clock_fit::fit::ClockFit;

// A remote clock that started 5 s before the host and runs 20 ppm fast.
let remote_at = |host_ns: u64| -> i64 {
    let host = host_ns as i64;
    host + 5_000_000_000 + host / 50_000
};

let mut fit = ClockFit::sixty_seconds();
for i in 0..30u64 {
    let host_ns = 1_000_000_000 + i * 1_000_000_000;
    fit.push(host_ns, remote_at(host_ns));
}

// Map a remote timestamp back onto the host clock.
let remote_ns = remote_at(20_500_000_000);
let host_ns = fit.to_host(remote_ns).unwrap();
assert!(host_ns.abs_diff(20_500_000_000) < 1_000);
assert!((fit.skew_ppm() - 20.0).abs() < 0.01);
println!("skew {:.2} ppm, sigma {} ns", fit.skew_ppm(), fit.sigma_ns());
```

## Highlights

- Two fits for the two kinds of time pair: least squares for pairs with symmetric error, and a lower envelope for pairs stamped on arrival, whose delay only ever adds.
- Offsets are kept against an integer reference, so a UTC-scale offset (about 1.8e18 ns) keeps nanosecond precision.
- Bounded memory: each fit holds a sliding window of pairs, capped by time and by count.
- A min-RTT gate for two-way exchanges (MAVLink TIMESYNC) drops queueing-delayed replies before they bias the offset.
- One dependency (`tracing`); 12 unit tests plus the README example.

## How it works

The crate has three modules.

[`fit`](https://docs.rs/ados-clock-fit/latest/ados_clock_fit/fit/index.html) holds [`ClockFit`](https://docs.rs/ados-clock-fit/latest/ados_clock_fit/fit/struct.ClockFit.html), a least-squares fit of `offset(host) = source - host` as a line in host time over the pairs of the last window. The line gives the offset and the skew; sigma is the RMS of the residuals and reads `-1` until three pairs exist. `to_source` and `to_host` convert in either direction. [`TimesyncEstimator`](https://docs.rs/ados-clock-fit/latest/ados_clock_fit/fit/struct.TimesyncEstimator.html) runs NTP-style two-way exchanges on top: each exchange gives the host send time, the remote reply time and the host receive time, and the remote time is taken to belong to the midpoint. Only exchanges whose round trip is within a margin of the minimum over the last 32 feed the fit.

[`envelope`](https://docs.rs/ados-clock-fit/latest/ados_clock_fit/envelope/index.html) holds [`EnvelopeFit`](https://docs.rs/ados-clock-fit/latest/ados_clock_fit/envelope/struct.EnvelopeFit.html), for pairs stamped when they reach the host (a remote time carried inside a video frame, for example). Encode, transport and decode latency only make `host - source` larger and are jittery, so the lower envelope of `host - source` is the true relation shifted by the pipeline's minimum latency, which one-way pairs cannot measure. The fit is the line on or above every pair of the window that minimises their summed lateness (the linear program of one-way delay skew estimation); its optimum is the edge of the upper convex hull that spans the mean host time. Only the least-late pair of each 50 ms host slot is kept, so the window spans the same time whatever the pair rate. An envelope is ready to trust once its pairs span 40 s.

[`chrony`](https://docs.rs/ados-clock-fit/latest/ados_clock_fit/chrony/index.html) (Unix only) encodes chrony's `struct sock_sample` and sends it to the socket chrony binds for a `refclock SOCK <path>` line, so a GNSS PPS pulse fitted here can discipline the system clock. The 40-byte layout with a 64-bit `struct timeval` is written explicitly, and [`ChronySink`](https://docs.rs/ados-clock-fit/latest/ados_clock_fit/chrony/struct.ChronySink.html) sends without blocking: an absent or full receiver drops the sample.

## Part of ADOS

Used in production by the [ADOS Drone Agent](https://github.com/altnautica/ADOSDroneAgent), the onboard software of the ADOS software-defined drone platform.
[All ADOS crates](https://github.com/altnautica/ADOSDroneAgent/tree/main/crates#published-crates) · [Docs](https://docs.altnautica.com) · [Discord](https://discord.gg/uxbvuD4d5q)

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this crate by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
