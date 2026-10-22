# ados-clock

[![crates.io](https://img.shields.io/crates/v/ados-clock.svg)](https://crates.io/crates/ados-clock)
[![docs.rs](https://img.shields.io/docsrs/ados-clock)](https://docs.rs/ados-clock)
[![license](https://img.shields.io/crates/l/ados-clock.svg)](#license)
[![MSRV](https://img.shields.io/badge/MSRV-1.93-blue.svg)](https://github.com/altnautica/ADOSDroneAgent/blob/main/crates/Cargo.toml)

**Map remote clocks onto a host monotonic timebase: windowed least-squares and lower-envelope fits, NTP-style TIMESYNC estimation and a chrony SOCK refclock sink.**

A flight controller, a sensor board or a GNSS receiver each stamp data with their own clock, which starts at a different moment and drifts at its own rate. To fuse that data you need every stamp on one timebase. This crate fits the relation between a remote clock and the host monotonic clock from pairs of readings, then converts timestamps in either direction. Reach for it when you log or fuse data from several devices on one computer.

```rust
use ados_clock::fit::ClockFit;

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

[`fit`](https://docs.rs/ados-clock/latest/ados_clock/fit/index.html) holds [`ClockFit`](https://docs.rs/ados-clock/latest/ados_clock/fit/struct.ClockFit.html), a least-squares fit of `offset(host) = source - host` as a line in host time over the pairs of the last window. The line gives the offset and the skew; sigma is the RMS of the residuals and reads `-1` until three pairs exist. `to_source` and `to_host` convert in either direction. [`TimesyncEstimator`](https://docs.rs/ados-clock/latest/ados_clock/fit/struct.TimesyncEstimator.html) runs NTP-style two-way exchanges on top: each exchange gives the host send time, the remote reply time and the host receive time, and the remote time is taken to belong to the midpoint. Only exchanges whose round trip is within a margin of the minimum over the last 32 feed the fit.

[`envelope`](https://docs.rs/ados-clock/latest/ados_clock/envelope/index.html) holds [`EnvelopeFit`](https://docs.rs/ados-clock/latest/ados_clock/envelope/struct.EnvelopeFit.html), for pairs stamped when they reach the host (a remote time carried inside a video frame, for example). Encode, transport and decode latency only make `host - source` larger and are jittery, so the lower envelope of `host - source` is the true relation shifted by the pipeline's minimum latency, which one-way pairs cannot measure. The fit is the line on or above every pair of the window that minimises their summed lateness (the linear program of one-way delay skew estimation); its optimum is the edge of the upper convex hull that spans the mean host time. Only the least-late pair of each 50 ms host slot is kept, so the window spans the same time whatever the pair rate. An envelope is ready to trust once its pairs span 40 s.

[`chrony`](https://docs.rs/ados-clock/latest/ados_clock/chrony/index.html) (Unix only) encodes chrony's `struct sock_sample` and sends it to the socket chrony binds for a `refclock SOCK <path>` line, so a GNSS PPS pulse fitted here can discipline the system clock. The 40-byte layout with a 64-bit `struct timeval` is written explicitly, and [`ChronySink`](https://docs.rs/ados-clock/latest/ados_clock/chrony/struct.ChronySink.html) sends without blocking: an absent or full receiver drops the sample.

## Part of ADOS

Used in production by the [ADOS Drone Agent](https://github.com/altnautica/ADOSDroneAgent), the onboard software of the ADOS software-defined drone platform.
[All ADOS crates](https://github.com/altnautica/ADOSDroneAgent/tree/main/crates#published-crates) · [Docs](https://docs.altnautica.com) · [Discord](https://discord.gg/uxbvuD4d5q)

### Other ADOS crates

<!-- ados-family:crates -->
**Platform (versioned with the ADOS Drone Agent)**

| Crate | What it does | Docs |
|---|---|---|
| [`ados-protocol`](https://crates.io/crates/ados-protocol) | Wire contracts of the ADOS Drone Agent: IPC framing, plugin RPC envelope, capability tokens and the vision frame bus. | [docs.rs](https://docs.rs/ados-protocol) |
| [`ados-sdk`](https://crates.io/crates/ados-sdk) | Write Rust plugins for the ADOS Drone Agent: IPC client, plugin context, driver traits and lifecycle runner. | [docs.rs](https://docs.rs/ados-sdk) |
| [`ados-vision`](https://crates.io/crates/ados-vision) | Single-object visual tracker: Kalman motion model, appearance re-identification and YOLOv8 head decoding. | [docs.rs](https://docs.rs/ados-vision) |
| [`ados-gpio`](https://crates.io/crates/ados-gpio) | Drive status buzzers and LEDs on Linux GPIO lines, with bounded software-PWM beep patterns. | [docs.rs](https://docs.rs/ados-gpio) |
| [`ados-hal-probe`](https://crates.io/crates/ados-hal-probe) | Find out what a board's hardware can really do by probing it, plus bundled board profiles. | [docs.rs](https://docs.rs/ados-hal-probe) |

**Standalone**

| Crate | What it does | Docs |
|---|---|---|
| [`ados-sensorhub`](https://crates.io/crates/ados-sensorhub) | Sensor hub wire protocol: COBS framing, CRC-16 and fixed-layout IMU, baro, mag and GNSS packets. no_std. | [docs.rs](https://docs.rs/ados-sensorhub) |
| [`ados-offload`](https://crates.io/crates/ados-offload) | Choose where perception runs (onboard, offloaded or hybrid) and hold the vehicle when results go stale. | [docs.rs](https://docs.rs/ados-offload) |
| [`ados-macpin`](https://crates.io/crates/ados-macpin) | Stable MAC addresses for Linux network adapters that randomize theirs every boot. | [docs.rs](https://docs.rs/ados-macpin) |
| [`ados-lora`](https://crates.io/crates/ados-lora) | LoRa transmit governor with cited regional band plans, duty cycle, dwell time and airtime accounting. | [docs.rs](https://docs.rs/ados-lora) |
| [`ados-crsf-codec`](https://crates.io/crates/ados-crsf-codec) | no_std CRSF / ExpressLRS codec for RC channels, telemetry and parameter frames. | [docs.rs](https://docs.rs/ados-crsf-codec) |
| [`ados-positioning`](https://crates.io/crates/ados-positioning) | Split mixed UBX, NMEA and RTCM3 GNSS streams, configure u-blox receivers and run an RTK base survey-in. | [docs.rs](https://docs.rs/ados-positioning) |
| [`ados-rtcm`](https://crates.io/crates/ados-rtcm) | RTCM3 framing, CRC-24Q and MAVLink GPS_RTCM_DATA fragmentation for RTK corrections. | [docs.rs](https://docs.rs/ados-rtcm) |
| [`ados-swarm`](https://crates.io/crates/ados-swarm) | Swarm control laws: separation, flocking, formations, CBBA task allocation and a point-mass simulator. | [docs.rs](https://docs.rs/ados-swarm) |
| [`ados-mavlink-tunnel`](https://crates.io/crates/ados-mavlink-tunnel) | Send large messages over MAVLink TUNNEL frames with chunking and bounded reassembly. | [docs.rs](https://docs.rs/ados-mavlink-tunnel) |
| [`ados-display-driver`](https://crates.io/crates/ados-display-driver) | Small-display output on Linux: SSD1306/SH1106 OLED over I2C, pixel packers and a framebuffer writer. | [docs.rs](https://docs.rs/ados-display-driver) |
| [`ados-hmi`](https://crates.io/crates/ados-hmi) | Front-panel input: touchscreen calibration and gestures, button presses and rotary encoders. | [docs.rs](https://docs.rs/ados-hmi) |
<!-- /ados-family:crates -->

### ADOS repositories

<!-- ados-family:repos -->
| Repo | Runs on | What it does | Link |
|---|---|---|---|
| ADOS Mission Control | Browser, desktop (Electron) | Ground control station: FC setup, mission planning, flight, 3D simulation, fleet | [ADOSMissionControl](https://github.com/altnautica/ADOSMissionControl) |
| ADOS Drone Agent | Companion computer on the drone, or a ground node | Onboard agent: MAVLink routing, HD video, long-range radio link, plugins, remote control | [ADOSDroneAgent](https://github.com/altnautica/ADOSDroneAgent) |
| ADOS Android GCS | Android phones and tablets | Native ground control app for agents on the LAN, direct flight controllers and the cloud | [ADOSAndroidGCS](https://github.com/altnautica/ADOSAndroidGCS) |
| ADOS Extensions | Drone Agent and Mission Control | First-party signed plugins, each with an agent half and a GCS half | [ADOSExtensions](https://github.com/altnautica/ADOSExtensions) |
| ADOS MCP | Your machine | Model Context Protocol server that lets AI clients operate ADOS drones under scopes and a safety gate | [ADOS-MCP](https://github.com/altnautica/ADOS-MCP) |
| Documentation | docs.altnautica.com | Public docs for every ADOS product | [Documentation](https://github.com/altnautica/Documentation) |
<!-- /ados-family:repos -->

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this crate by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
