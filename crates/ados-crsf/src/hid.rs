//! The HID/PIC channel source: stick + switch intent from the primary
//! gamepad, fed into the source merge.
//!
//! Device selection follows the input stack's existing seams — the
//! `ados-input` daemon's command socket answers `get_primary` with the bound
//! gamepad's device id (`usb:<vid>:<pid>:<eventN>`, where the basename is the
//! evdev node), and the PIC arbiter's state sidecar (read by the heartbeat,
//! see the `sources` module) gates whose input flies in hybrid mode. Only the
//! evdev node-open + read loop is Linux-gated; the axis/switch → channel
//! maths and the mapping tables are pure and host-tested.
//!
//! Default mapping (mode-2 AETR): right stick X → roll (ch 0), right stick Y
//! → pitch (ch 1, inverted — pushing forward is nose-down positive), left
//! stick Y → throttle (ch 2, inverted — up is more), left stick X → yaw
//! (ch 3); the eight face/shoulder buttons ride the aux channels 4..=11 as
//! two-position switches.
//!
//! # Liveness
//!
//! evdev delivers edges only, and the kernel's fuzz/flat filtering keeps a
//! stick held steady completely silent, so silence on its own says nothing
//! about the device. The reader therefore polls the device's current axis and
//! button state from the kernel on [`crate::sources::HID_LIVENESS_INTERVAL`]:
//! a successful poll re-stamps the merge with the polled values, so a pilot
//! holding the sticks still keeps authority. A poll error, a read error
//! (including a dropped-event resync the kernel refused) or the device going
//! away ends the session and clears the slot at once; the lane then transmits
//! nothing, so the receiver's failsafe runs. The frame is seeded from a poll
//! at open, so a stick already deflected is reported as it is.

use std::future::Future;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use ados_crsf_codec::channels::{CHANNEL_COUNT, CHANNEL_MID};
use ados_crsf_codec::scale::{axis_to_channel, switch_to_channel};

#[cfg(target_os = "linux")]
mod linux;
mod map;
mod primary;

#[cfg(target_os = "linux")]
pub use linux::*;
pub use map::*;
pub use primary::*;

/// One observation from a device's event stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HidInput {
    /// An absolute-axis sample: evdev ABS code and raw value.
    Abs { code: u16, value: i32 },
    /// A key/button edge: evdev KEY code and whether it is down.
    Key { code: u16, pressed: bool },
    /// Anything the channel map does not consume (sync reports, misc).
    Other,
}

/// The device's current state as one kernel poll read it, in the order of
/// [`DEFAULT_AXIS_MAP`] and [`DEFAULT_BUTTON_MAP`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HidSnapshot {
    /// Raw value of each mapped axis.
    pub axes: [i32; DEFAULT_AXIS_MAP.len()],
    /// Whether each mapped button is down.
    pub buttons: [bool; DEFAULT_BUTTON_MAP.len()],
}

/// An open input device as the reader sees it: an event stream plus a direct
/// state poll. The evdev node implements it on Linux.
pub trait HidDevice {
    /// The next event. An error means the device is gone or its stream can no
    /// longer be trusted (a dropped-event resync the kernel refused).
    fn next_input(&mut self) -> impl Future<Output = std::io::Result<HidInput>> + Send + '_;

    /// The device's current axis and button state, read from the kernel. An
    /// error means the device no longer answers.
    fn poll_state(&self) -> std::io::Result<HidSnapshot>;
}

/// Why a device session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaderExit {
    /// Shutdown latched.
    Shutdown,
    /// The device failed a read or a poll, or the primary selection moved.
    DeviceLost,
}

/// Re-stamp the merge's HID slot with the frame.
async fn push_frame(
    merge: &std::sync::Arc<tokio::sync::Mutex<crate::sources::SourceMerge>>,
    frame: &HidChannels,
) {
    let now = std::time::Instant::now();
    if let Err(e) = merge.lock().await.set_hid(frame.values(), now) {
        // Unreachable through the clamped scaler; loud if the invariant ever
        // breaks.
        tracing::error!(error = ?e, "hid_scaled_value_rejected");
    }
}

/// Stream one open device into the merge until it fails a read or a poll, or
/// `stop` resolves.
///
/// The frame is seeded from a poll before the first push. Every changed
/// channel is pushed as it arrives, and every successful
/// [`crate::sources::HID_LIVENESS_INTERVAL`] poll re-stamps the slot with the
/// polled state, so a device held perfectly still stays live while one that
/// stops answering is dropped on the next poll. The HID slot is cleared on
/// every exit: its last stick never outlives the session.
pub async fn drive_device<D: HidDevice>(
    device: &mut D,
    cal: &[AxisCal],
    merge: &std::sync::Arc<tokio::sync::Mutex<crate::sources::SourceMerge>>,
    stop: impl Future<Output = ReaderExit>,
) -> ReaderExit {
    let exit = drive_until_exit(device, cal, merge, stop).await;
    merge.lock().await.clear_hid();
    exit
}

async fn drive_until_exit<D: HidDevice>(
    device: &mut D,
    cal: &[AxisCal],
    merge: &std::sync::Arc<tokio::sync::Mutex<crate::sources::SourceMerge>>,
    stop: impl Future<Output = ReaderExit>,
) -> ReaderExit {
    let mut frame = HidChannels::default();
    match device.poll_state() {
        Ok(snapshot) => frame.apply_snapshot(&snapshot, cal),
        Err(e) => {
            tracing::warn!(error = %e, "hid_device_poll_failed");
            return ReaderExit::DeviceLost;
        }
    }
    push_frame(merge, &frame).await;

    let mut liveness_tick = tokio::time::interval(crate::sources::HID_LIVENESS_INTERVAL);
    liveness_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick completes at once; the open-time poll above covered it.
    liveness_tick.tick().await;
    tokio::pin!(stop);
    loop {
        tokio::select! {
            biased;
            exit = &mut stop => return exit,
            _ = liveness_tick.tick() => match device.poll_state() {
                Ok(snapshot) => {
                    frame.apply_snapshot(&snapshot, cal);
                    push_frame(merge, &frame).await;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "hid_device_poll_failed");
                    return ReaderExit::DeviceLost;
                }
            },
            ev = device.next_input() => match ev {
                Ok(input) => {
                    if frame.apply_input(input, cal) {
                        push_frame(merge, &frame).await;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "hid_device_lost");
                    return ReaderExit::DeviceLost;
                }
            },
        }
    }
}

#[cfg(test)]
mod tests;
