//! Human-interface devices: buttons, touch, the PIC arbiter, and gamepad input.
//!
//! The pure input state machines (touch affine calibration and gestures, the
//! pilot-in-command arbiter, the event buses, press classification and
//! quadrature decoding) live in the `ados-hmi` crate.
//!
//! * [`pic_ipc`] — the PIC IPC seam: the Unix-domain control socket through
//!   which other processes reach the single arbiter instance the `ados-pic`
//!   daemon owns (closes the per-process split-brain).
//! * [`buttons`] — the GPIO front-panel button and rotary-encoder service: the
//!   profile mapping (SIGHUP merge) plus a target-gated character-device read loop.
//! * [`buttons_ipc`] — a dedicated Unix-domain fanout socket
//!   (`/run/ados/buttons.sock`) streaming already-classified front-panel presses
//!   as newline-JSON, the seam the HDMI cockpit relay subscribes to (sibling of
//!   the pic.sock `subscribe_buttons` stream, sharing the same button bus).
//! * [`input`] — gamepad enumeration, the 1 Hz hotplug diff engine, and primary
//!   persistence; the diff engine + gamepad predicate are pure, evdev gated.
//! * [`sidecar`] — `/etc/ados` sidecar persistence (touch.calib path +
//!   `ground-station-input.json`).
//! * [`pic_sidecar`] — the PIC arbiter state sidecar (`/run/ados/pic-state.json`)
//!   the `ados-pic` daemon mirrors the arbiter snapshot to on every transition,
//!   so a reader (the display layer, a status route) has the live holder without
//!   a socket round-trip.
//! * [`hid_cmd`] — the `ados-input` daemon's operator command socket
//!   (`/run/ados/hid-cmd.sock`): the write seam for the primary-gamepad selection,
//!   applied through the running hotplug tracker (the single owner of the live
//!   primary) so a selection takes effect without a daemon restart.
//! * [`paths`] — the runtime path constants the PIC state sidecar + the input
//!   command socket resolve under.
//!
//! The OLED/HDMI display layers (the `ados-display` crate) land separately.

pub mod buttons;
pub mod buttons_ipc;
#[cfg(unix)]
pub mod daemon;
pub mod hid_cmd;
pub mod input;
pub mod paths;
pub mod pic_ipc;
pub mod pic_sidecar;
pub mod pic_view;
pub mod sidecar;
