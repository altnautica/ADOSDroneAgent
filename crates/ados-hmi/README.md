# ados-hmi

[![crates.io](https://img.shields.io/crates/v/ados-hmi.svg)](https://crates.io/crates/ados-hmi)
[![docs.rs](https://img.shields.io/docsrs/ados-hmi)](https://docs.rs/ados-hmi)
[![license](https://img.shields.io/crates/l/ados-hmi.svg)](#license)
[![MSRV](https://img.shields.io/badge/MSRV-1.93-blue.svg)](https://github.com/altnautica/ADOSDroneAgent/blob/main/crates/Cargo.toml)

**Input state machines for hardware front panels: touchscreen affine calibration and gestures, button press classification, quadrature encoder decoding and a pilot-in-command arbiter.**

A small device with a few buttons, a rotary knob and a resistive touchscreen needs the same logic every time: debounce the buttons, tell a short press from a long one, turn noisy encoder edges into clean detents, map raw touch counts onto display pixels and recognise taps and swipes. This crate holds those pieces as pure state machines that take timestamped events, so you wire them to GPIO or evdev yourself and test them without hardware.

```rust
use std::collections::HashMap;
use ados_hmi::encoder::{Channel, QuadratureDecoder, Rotation};
use ados_hmi::press::{Edge, PressClassifier, PressKind};

// Four buttons on GPIO 5, 6, 13 and 19 are labelled B1..B4.
let mapping: HashMap<String, String> = [("B1_short", "next"), ("B1_long", "menu")]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
let mut buttons = PressClassifier::from_mapping(mapping, [5, 6, 13, 19]);

// Press at t=0 ms, a contact bounce 8 ms later, release after 2.5 s: one long press.
assert!(buttons.on_edge(5, Edge::Falling, 0).is_none());
assert!(buttons.on_edge(5, Edge::Falling, 8).is_none());
let event = buttons.on_edge(5, Edge::Rising, 2_500).unwrap();
assert_eq!(event.kind, PressKind::Long);
assert_eq!(event.action.as_deref(), Some("menu"));

// One clockwise detent of a KY-040 style encoder: A leads B.
let mut knob = QuadratureDecoder::new(true, true);
let mut detents = Vec::new();
for (line, high) in [(Channel::A, false), (Channel::B, false), (Channel::A, true), (Channel::B, true)] {
    detents.extend(knob.on_edge(line, high));
}
assert_eq!(detents, [Rotation::Cw]);
```

## Highlights

- Every state machine takes caller-supplied timestamps or an injectable clock, so debounce windows, hold thresholds, token TTLs and watchdogs are tested without sleeping.
- Quadrature decoding counts quarter steps, so contact bounce cancels out and a missed edge is rejected instead of producing a wrong detent.
- Touch calibration fits a 2x3 affine matrix by least squares from any number of points and reports the RMS residual.
- Action mappings sit behind a shared `Arc<RwLock<_>>`, so a remap takes effect on the next release without rebuilding the classifier.
- Event buses are bounded `tokio::sync::broadcast` channels: a slow subscriber lags instead of stalling the publisher.
- Builds on any host; nothing touches GPIO, evdev or a display directly.

## How it works

- [`press`](https://docs.rs/ados-hmi/latest/ados_hmi/press/): active-low buttons. A falling edge records the press unless it lands inside the 20 ms debounce window of the previous press on that pin. The rising edge classifies the hold: under 2 s is `short`, 2 s to 6 s is `long`, 6 s or more is dropped as a cancel. The action is looked up as `<label>_<kind>`, where the label is `B1`..`B4` by role order, `BX<pin>` for other pins, or `ENC_SW` for an encoder push switch.
- [`encoder`](https://docs.rs/ados-hmi/latest/ados_hmi/encoder/): the 2-bit Gray code `A << 1 | B` walks four states per detent. Each valid single-line transition adds or removes a quarter step; a detent is reported only on the return to rest with a net count of four in one direction. A transition where both lines changed is counted as rejected. Detents can be fed into `PressClassifier::on_rotation` and resolve as `ENC_CW_short` / `ENC_CCW_short`.
- [`affine`](https://docs.rs/ados-hmi/latest/ados_hmi/affine/): maps raw 12-bit touch ADC counts to display pixels. Covers the four rotation identities, the normal-equations least-squares fit, a Gaussian-elimination solver with partial pivot and singular guard, round-half-to-even application, and atomic JSON persistence that rejects a stored calibration whose panel size or rotation no longer matches.
- [`touch`](https://docs.rs/ados-hmi/latest/ados_hmi/touch/): a per-stroke state machine with a move filter, classifying each stroke as tap, long press, swipe or drag by duration and displacement, with direction taken from the larger axis.
- [`pic`](https://docs.rs/ados-hmi/latest/ados_hmi/pic/): a pilot-in-command arbiter. Exactly one client holds control; a takeover needs a confirm token or a force flag, a missed heartbeat auto-releases, and a connected gamepad can auto-claim an unheld seat. Each transition is published on [`eventbus`](https://docs.rs/ados-hmi/latest/ados_hmi/eventbus/).

## Part of ADOS

Used in production by the [ADOS Drone Agent](https://github.com/altnautica/ADOSDroneAgent), the onboard software of the ADOS software-defined drone platform.
[All ADOS crates](https://github.com/altnautica/ADOSDroneAgent/tree/main/crates#published-crates) · [Docs](https://docs.altnautica.com) · [Discord](https://discord.gg/uxbvuD4d5q)

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this crate by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
