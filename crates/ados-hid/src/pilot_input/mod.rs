//! Native controller flight on the ground station (DualShock 4 and any
//! standard pad): sticks and the safety buttons go to the drone's pilot through
//! the ground station's relay socket, the lightbar and rumble follow the drone's
//! pilot state, and the binding file from Settings › Controls drives the
//! native gestures.
//!
//! * [`spec`] — the cockpit-spec controller table, compiled in.
//! * [`pad`] — evdev state onto controls, conditioning, the `buttons` bitfield.
//! * [`gestures`] — tap/hold bindings with combos and conditions.
//! * [`bindings`] — the binding document, its locked-action check, the native subset.
//! * [`feedback`] — lightbar state and colour, rumble cues and the pulse queue.
//! * [`status`] — control mode, the emergency latch, the runtime sidecar.
//! * [`wire`] — `pilot.v1` frames.
//! * `linux`, `runner` — the device IO and the 50 Hz loop (Linux only).

pub mod bindings;
pub mod feedback;
pub mod gestures;
#[cfg(target_os = "linux")]
pub mod linux;
pub mod pad;
#[cfg(target_os = "linux")]
pub mod runner;
pub mod spec;
pub mod status;
pub mod wire;
