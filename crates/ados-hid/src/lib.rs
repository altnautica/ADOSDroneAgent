//! Human-interface devices: buttons, touch, the PIC arbiter, and gamepad input.
//!
//! * [`affine`] — touchscreen affine calibration (raw ADC -> LCD pixels):
//!   the matrix, the four rotation identities, the least-squares fit, and the
//!   atomic `touch.calib` persistence.
//! * [`pic`] — the pilot-in-command arbiter finite state machine, with an
//!   injectable clock so the confirm-token TTL and the heartbeat watchdog are
//!   testable without sleeping.
//! * [`pic_ipc`] — the PIC IPC seam: the Unix-domain control socket through
//!   which other processes reach the single arbiter instance the `ados-pic`
//!   daemon owns (closes the per-process split-brain).
//! * [`eventbus`] — the PIC transition fanout bus (drop-on-full broadcast).
//! * [`buttons`] — the GPIO front-panel button service: pure press
//!   classification (debounce, short/long/cancel, SIGHUP mapping merge) plus a
//!   target-gated character-device read loop.
//! * [`sidecar`] — `/etc/ados` sidecar persistence (touch.calib path +
//!   `ground-station-input.json`).
//!
//! The touch input layer and the OLED/HDMI display layers land in later chunks.

pub mod affine;
pub mod buttons;
pub mod eventbus;
pub mod pic;
pub mod pic_ipc;
pub mod sidecar;
