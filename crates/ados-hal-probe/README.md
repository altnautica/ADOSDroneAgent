# ados-hal-probe

[![crates.io](https://img.shields.io/crates/v/ados-hal-probe.svg)](https://crates.io/crates/ados-hal-probe)
[![docs.rs](https://img.shields.io/docsrs/ados-hal-probe)](https://docs.rs/ados-hal-probe)
[![license](https://img.shields.io/crates/l/ados-hal-probe.svg)](#license)
[![MSRV](https://img.shields.io/badge/MSRV-1.93-blue.svg)](https://github.com/altnautica/ADOSDroneAgent/blob/main/crates/Cargo.toml)

**Probe-first hardware capability detection for Linux single-board computers: CPU, SoC, USB, serial, V4L2 encoders and board profiles, read from the real silicon by the ADOS Drone Agent.**

A board description can be wrong, `ffmpeg -encoders` lists wrapper codecs with no device behind them, and a vendor tool can advertise a feature it cannot deliver. This crate derives capabilities by probing the hardware itself: it reads sysfs and the device tree, issues V4L2 ioctls, or runs a bounded trial initialisation, and records the evidence behind each answer. Use it when software must decide what a board can really do before it relies on it.

```rust
use ados_hal_probe::board_sidecar::{detect_tier, load_profiles, match_profile};

// The bundled board profiles parse without any hardware attached.
let profiles = load_profiles();
assert!(!profiles.is_empty());

// Match a device-tree model string to its profile.
let board = match_profile(&profiles, "Raspberry Pi 5 Model B Rev 1.0").unwrap();
assert_eq!(board.soc, "BCM2712");
assert_eq!(board.stem, "rpi5");

// RAM decides the performance tier.
assert_eq!(detect_tier(8192), 4);
```

## Highlights

- Capabilities come from probes, each paired with the evidence that produced it, not from declarations.
- Risky trial initialisations run in their own process group, which is killed as a whole on timeout, so a device stuck in an uninterruptible ioctl cannot hang the caller.
- Destructive probes run only in phases that allow them; read-only probes run in any phase.
- Board profiles are bundled in the crate and parsed at runtime, so detection needs nothing but the binary.

## How it works

The result types live in [`ados_protocol::hwcaps`](https://docs.rs/ados-protocol/latest/ados_protocol/hwcaps/index.html) and are re-exported here; this crate owns the I/O.

- [`probe`](https://docs.rs/ados-hal-probe/latest/ados_hal_probe/probe/index.html): read-only probes for CPU core clusters ([`probe_cores`](https://docs.rs/ados-hal-probe/latest/ados_hal_probe/probe/cpu/fn.probe_cores.html)), the SoC compatible string, USB ids, serial ports and the H.264 encoder ([`probe_h264_encoder`](https://docs.rs/ados-hal-probe/latest/ados_hal_probe/probe/video/fn.probe_h264_encoder.html)).
- [`runner`](https://docs.rs/ados-hal-probe/latest/ados_hal_probe/runner/index.html): runs a trial in a separate process group and sends `SIGKILL` to the whole group on timeout.
- [`board_sidecar`](https://docs.rs/ados-hal-probe/latest/ados_hal_probe/board_sidecar/index.html): the bundled board profiles, profile matching (override, device-tree compatible, model string, `/proc/cpuinfo`, in that order), RAM tiers and the resulting board fingerprint document.
- [`camera_probe`](https://docs.rs/ados-hal-probe/latest/ados_hal_probe/camera_probe/index.html) and [`sensor_hub`](https://docs.rs/ados-hal-probe/latest/ados_hal_probe/sensor_hub/index.html): camera and USB sensor hub presence.

Probe phases: stateful probes such as encoder trial initialisation run only when the [`ProbePhase`](https://docs.rs/ados-protocol/latest/ados_protocol/hwcaps/enum.ProbePhase.html) permits it (`Setup` or `BootPreArm`, never `Runtime` while armed).

```rust,no_run
// Real probing reads the running host.
let cores = ados_hal_probe::probe_cores();
println!("{cores:?}");
```

## Part of ADOS

Used in production by the [ADOS Drone Agent](https://github.com/altnautica/ADOSDroneAgent), the onboard software of the ADOS software-defined drone platform.
[All ADOS crates](https://github.com/altnautica/ADOSDroneAgent/tree/main/crates#published-crates) · [Docs](https://docs.altnautica.com) · [Discord](https://discord.gg/uxbvuD4d5q)

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this crate by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
