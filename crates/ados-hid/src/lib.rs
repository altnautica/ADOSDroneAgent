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
//! * [`touch`] — the touch stroke FSM + tap/long_press/swipe/drag classifier
//!   (pure), fed by the affine transform; the evdev read loop is target-gated.
//! * [`input`] — gamepad enumeration, the 1 Hz hotplug diff engine, and primary
//!   persistence; the diff engine + gamepad predicate are pure, evdev gated.
//! * [`sidecar`] — `/etc/ados` sidecar persistence (touch.calib path +
//!   `ground-station-input.json`).
//!
//! The OLED/HDMI display layers (the `ados-display` crate) land separately.

pub mod affine;
pub mod buttons;
pub mod eventbus;
pub mod input;
pub mod pic;
pub mod pic_ipc;
pub mod sidecar;
pub mod touch;
