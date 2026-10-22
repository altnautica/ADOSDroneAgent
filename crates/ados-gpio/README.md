# ados-gpio

[![crates.io](https://img.shields.io/crates/v/ados-gpio.svg)](https://crates.io/crates/ados-gpio)
[![docs.rs](https://img.shields.io/docsrs/ados-gpio)](https://docs.rs/ados-gpio)
[![license](https://img.shields.io/crates/l/ados-gpio.svg)](#license)
[![MSRV](https://img.shields.io/badge/MSRV-1.93-blue.svg)](https://github.com/altnautica/ADOSDroneAgent/blob/main/crates/Cargo.toml)

**Drive a buzzer or LED on a Linux GPIO character-device line and compute bounded software-PWM beep schedules, as used by the ADOS Drone Agent.**

A status buzzer or LED on a single-board computer needs two things: a way to drive a GPIO line, and a beep pattern that can never leave the line stuck high. This crate keeps the timing maths pure and testable on any host, and puts the hardware write behind a small Linux-only type. Reach for it when a service must signal state through a buzzer or LED and must stay safe under malformed commands.

```rust
use ados_gpio::{beep_schedule, BeepPattern, Level, MAX_TOTAL_MS};

// Three short beeps with a 2 kHz carrier at 50 % duty.
let pattern = BeepPattern { freq_hz: 2_000, duty_pct: 50, on_ms: 100, off_ms: 150, cycles: 3 };
let schedule = beep_schedule(pattern);

assert_eq!(schedule[0].level, Level::High);
assert_eq!(schedule[0].hold_ms, 100);
// The schedule always ends with the line low.
assert_eq!(schedule.last().unwrap().level, Level::Low);

// A hostile pattern is clamped and truncated to the total budget.
let huge = BeepPattern { freq_hz: 0, duty_pct: 100, on_ms: u32::MAX, off_ms: u32::MAX, cycles: u32::MAX };
let total: u64 = beep_schedule(huge).iter().map(|p| u64::from(p.hold_ms)).sum();
assert!(total <= MAX_TOTAL_MS);
```

## Highlights

- Safe by default: every pattern is clamped ([`BeepPattern::clamp`](https://docs.rs/ados-gpio/latest/ados_gpio/struct.BeepPattern.html#method.clamp)) and every schedule is capped at `MAX_TOTAL_MS` and ends low.
- Timing and command parsing are pure functions with no hardware or clock, unit-tested on any host.
- The hardware half uses the pure-Rust `gpio-cdev` backend, so there is no libgpiod C dependency.
- Linux-only items are gated with `cfg(target_os = "linux")`; the rest builds on any platform.

## How it works

The crate has two halves.

- **Hardware output.** [`GpioOutput`](https://docs.rs/ados-gpio/latest/ados_gpio/struct.GpioOutput.html) opens a `/dev/gpiochip*` line in output mode and drives it high or low. It exists only on Linux.
- **Portable maths.** [`BeepPattern`](https://docs.rs/ados-gpio/latest/ados_gpio/struct.BeepPattern.html), [`beep_schedule`](https://docs.rs/ados-gpio/latest/ados_gpio/fn.beep_schedule.html) and [`carrier_timing`](https://docs.rs/ados-gpio/latest/ados_gpio/fn.carrier_timing.html) turn a pattern into a list of [`BeepPhase`](https://docs.rs/ados-gpio/latest/ados_gpio/struct.BeepPhase.html) toggle instants plus the carrier half-periods. A software-PWM beep is that list applied to a `GpioOutput`.

Commands arrive as one newline-delimited JSON request per connection. [`parse_command`](https://docs.rs/ados-gpio/latest/ados_gpio/fn.parse_command.html) validates a request into a [`Command`](https://docs.rs/ados-gpio/latest/ados_gpio/enum.Command.html) or a stable error code, without touching hardware. A fresh service drives no line until it receives an explicit command.

Bounds: carrier up to `MAX_CARRIER_HZ` (5 kHz), each phase up to `MAX_PHASE_MS` (5 s), up to `MAX_BEEP_CYCLES` (64) cycles, and a whole schedule of at most `MAX_TOTAL_MS` (30 s).

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
| [`ados-clock`](https://crates.io/crates/ados-clock) | Map remote clocks onto the host clock: least-squares and envelope fits, TIMESYNC and a chrony sink. | [docs.rs](https://docs.rs/ados-clock) |
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
