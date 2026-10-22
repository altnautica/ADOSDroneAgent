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

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this crate by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
