//! Human-interface devices: buttons, touch, the PIC arbiter, and gamepad input.
//!
//! This is the pure-logic core of the HID layer, with zero hardware
//! dependencies:
//!
//! * [`affine`] — touchscreen affine calibration (raw ADC -> LCD pixels):
//!   the matrix, the four rotation identities, the least-squares fit, and the
//!   atomic `touch.calib` persistence.
//! * [`pic`] — the pilot-in-command arbiter finite state machine, with an
//!   injectable clock so the confirm-token TTL and the heartbeat watchdog are
//!   testable without sleeping.
//! * [`eventbus`] — the PIC transition fanout bus (drop-on-full broadcast).
//! * [`sidecar`] — `/etc/ados` sidecar persistence (touch.calib path +
//!   `ground-station-input.json`).
//!
//! The device layers (gpio button reads, evdev gamepad input, the `ados-pic`
//! daemon, and the IPC seam) land in later chunks on top of this core.

pub mod affine;
pub mod eventbus;
pub mod pic;
pub mod sidecar;
