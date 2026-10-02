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

use crate::channels::{CHANNEL_COUNT, CHANNEL_MID};
use crate::scale::{axis_to_channel, switch_to_channel};

/// One absolute-axis mapping: an evdev ABS code onto a channel index.
#[derive(Debug, Clone, Copy)]
pub struct AxisMap {
    pub code: u16,
    pub channel: usize,
    pub inverted: bool,
}

/// One key/button mapping: an evdev KEY code onto an aux channel index.
#[derive(Debug, Clone, Copy)]
pub struct ButtonMap {
    pub code: u16,
    pub channel: usize,
}

/// The default stick map (evdev ABS codes; mode-2 AETR channel order).
pub const DEFAULT_AXIS_MAP: [AxisMap; 4] = [
    // ABS_RX → roll.
    AxisMap {
        code: 0x03,
        channel: 0,
        inverted: false,
    },
    // ABS_RY → pitch (stick forward = nose down = channel low on the wire's
    // convention; evdev Y grows downward, so forward is negative → invert).
    AxisMap {
        code: 0x04,
        channel: 1,
        inverted: true,
    },
    // ABS_Y → throttle (evdev Y grows downward; stick up = more throttle).
    AxisMap {
        code: 0x01,
        channel: 2,
        inverted: true,
    },
    // ABS_X → yaw.
    AxisMap {
        code: 0x00,
        channel: 3,
        inverted: false,
    },
];

/// The default switch map: the eight face/shoulder buttons onto aux channels
/// 4..=11 as two-position switches (evdev BTN_SOUTH..BTN_START codes).
pub const DEFAULT_BUTTON_MAP: [ButtonMap; 8] = [
    ButtonMap {
        code: 0x130, // BTN_SOUTH
        channel: 4,
    },
    ButtonMap {
        code: 0x131, // BTN_EAST
        channel: 5,
    },
    ButtonMap {
        code: 0x133, // BTN_NORTH
        channel: 6,
    },
    ButtonMap {
        code: 0x134, // BTN_WEST
        channel: 7,
    },
    ButtonMap {
        code: 0x136, // BTN_TL
        channel: 8,
    },
    ButtonMap {
        code: 0x137, // BTN_TR
        channel: 9,
    },
    ButtonMap {
        code: 0x13A, // BTN_SELECT
        channel: 10,
    },
    ButtonMap {
        code: 0x13B, // BTN_START
        channel: 11,
    },
];

/// A per-axis calibration read from the device's absinfo at open time.
#[derive(Debug, Clone, Copy)]
pub struct AxisCal {
    pub code: u16,
    pub min: i32,
    pub max: i32,
}

/// Scale a raw absolute-axis sample with its device range onto a CRSF channel
/// value: `min` → 172, `max` → 1811, the range midpoint → ~992 (exact when
/// the range is symmetric around zero). Degenerate ranges read center.
pub fn abs_to_channel(raw: i32, min: i32, max: i32, inverted: bool) -> u16 {
    if max <= min {
        return CHANNEL_MID;
    }
    let span = i64::from(max) - i64::from(min);
    let offset = i64::from(raw.clamp(min, max)) - i64::from(min);
    // Onto the full signed evdev-style range, then through the shared
    // two-half scaler so every axis uses the same center-exact maths.
    let mut scaled = (offset * 65_535 / span) - 32_768;
    if inverted {
        scaled = -scaled;
    }
    axis_to_channel(scaled.clamp(-32_768, 32_767) as i32)
}

/// The HID source's live channel frame: starts from the default bank, then
/// follows the device's polled state and events.
#[derive(Debug, Clone)]
pub struct HidChannels {
    values: [u16; CHANNEL_COUNT],
}

impl Default for HidChannels {
    fn default() -> Self {
        Self {
            values: crate::bank::ChannelBank::default().values(),
        }
    }
}

impl HidChannels {
    pub fn values(&self) -> [u16; CHANNEL_COUNT] {
        self.values
    }

    /// Apply an absolute-axis sample through the default map + the device
    /// calibration. Returns whether any channel changed.
    pub fn apply_abs(&mut self, code: u16, raw: i32, cal: &[AxisCal]) -> bool {
        let Some(map) = DEFAULT_AXIS_MAP.iter().find(|m| m.code == code) else {
            return false;
        };
        let Some(cal) = cal.iter().find(|c| c.code == code) else {
            return false;
        };
        let value = abs_to_channel(raw, cal.min, cal.max, map.inverted);
        if self.values[map.channel] == value {
            return false;
        }
        self.values[map.channel] = value;
        true
    }

    /// Apply a key/button edge through the default switch map. Returns whether
    /// any channel changed.
    pub fn apply_key(&mut self, code: u16, pressed: bool) -> bool {
        let Some(map) = DEFAULT_BUTTON_MAP.iter().find(|m| m.code == code) else {
            return false;
        };
        let value = switch_to_channel(u8::from(pressed), 2);
        if self.values[map.channel] == value {
            return false;
        }
        self.values[map.channel] = value;
        true
    }

    /// Apply one stream observation. Returns whether any channel changed.
    pub fn apply_input(&mut self, input: HidInput, cal: &[AxisCal]) -> bool {
        match input {
            HidInput::Abs { code, value } => self.apply_abs(code, value, cal),
            HidInput::Key { code, pressed } => self.apply_key(code, pressed),
            HidInput::Other => false,
        }
    }

    /// Apply a full polled device state. Axes without a calibration (the
    /// device does not report them) keep their current value.
    pub fn apply_snapshot(&mut self, snapshot: &HidSnapshot, cal: &[AxisCal]) {
        for (map, &raw) in DEFAULT_AXIS_MAP.iter().zip(snapshot.axes.iter()) {
            self.apply_abs(map.code, raw, cal);
        }
        for (map, &pressed) in DEFAULT_BUTTON_MAP.iter().zip(snapshot.buttons.iter()) {
            self.apply_key(map.code, pressed);
        }
    }
}

/// Resolve a gamepad device id (`usb:<vid>:<pid>:<eventN>`) to its evdev node
/// path. The trailing segment IS the `/dev/input` basename.
pub fn device_path_from_id(device_id: &str) -> Option<String> {
    let basename = device_id.rsplit(':').next()?;
    if !basename.starts_with("event") || basename.len() <= 5 {
        return None;
    }
    if !basename[5..].bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(format!("/dev/input/{basename}"))
}

/// The `ados-input` daemon's command socket under the resolved run dir.
pub fn hid_cmd_sock_path() -> PathBuf {
    PathBuf::from(crate::paths::run_path("hid-cmd.sock"))
}

/// Ask the input daemon which gamepad is primary: one newline-JSON
/// `get_primary` round-trip. `None` when the socket is unreachable, the reply
/// is unparseable/`ok:false`, or no primary is bound.
pub async fn query_primary(socket: &Path) -> Option<String> {
    const MAX_REPLY_BYTES: usize = 64 * 1024;

    let request = json!({"op": "get_primary"});
    let mut stream = tokio::net::UnixStream::connect(socket).await.ok()?;
    let line = format!("{}\n", serde_json::to_string(&request).ok()?);
    stream.write_all(line.as_bytes()).await.ok()?;
    stream.flush().await.ok()?;

    let mut raw = Vec::new();
    let mut buf = [0u8; 8 * 1024];
    loop {
        let n = stream.read(&mut buf).await.ok()?;
        if n == 0 {
            break; // EOF: the server replies once then closes.
        }
        if raw.len() + n > MAX_REPLY_BYTES {
            return None;
        }
        raw.extend_from_slice(&buf[..n]);
        if raw.contains(&b'\n') {
            break;
        }
    }
    let text = String::from_utf8(raw).ok()?;
    let reply: Value = serde_json::from_str(text.lines().next()?).ok()?;
    if reply.get("ok") != Some(&Value::Bool(true)) {
        return None;
    }
    reply
        .get("primary_id")
        .and_then(Value::as_str)
        .map(str::to_string)
}

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

/// How often the reader re-resolves the primary gamepad (and notices a
/// selection change) while idle or while a device is open.
#[cfg(target_os = "linux")]
const RESOLVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Await the latched shutdown flag, dropping the watch guard inside so the
/// wrapping future stays `Send` (the raw `wait_for` future's output holds a
/// lock guard the spawned task must not carry).
#[cfg(target_os = "linux")]
async fn wait_flag(rx: &mut tokio::sync::watch::Receiver<bool>) {
    let _ = rx.wait_for(|s| *s).await;
}

/// Run the HID source: resolve the primary gamepad, open its evdev node,
/// stream stick/switch events into the merge, and clear the merge's HID slot
/// the moment the device (or the reader) goes away. Retries forever on a
/// missing device; returns only on the latched shutdown.
#[cfg(target_os = "linux")]
pub async fn run_hid_source(
    merge: std::sync::Arc<tokio::sync::Mutex<crate::sources::SourceMerge>>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        if *shutdown.borrow() {
            merge.lock().await.clear_hid();
            return;
        }
        let primary = query_primary(&hid_cmd_sock_path()).await;
        let node = primary.as_deref().and_then(device_path_from_id);
        let Some(node) = node else {
            merge.lock().await.clear_hid();
            tokio::select! {
                biased;
                _ = wait_flag(&mut shutdown) => { merge.lock().await.clear_hid(); return; }
                _ = tokio::time::sleep(RESOLVE_INTERVAL) => continue,
            }
        };
        let primary = primary.unwrap_or_default();
        match stream_device(&node, &primary, &merge, &mut shutdown).await {
            ReaderExit::Shutdown => {
                merge.lock().await.clear_hid();
                return;
            }
            ReaderExit::DeviceLost => {
                // The last stick must not outlive the device that produced it.
                merge.lock().await.clear_hid();
                tokio::select! {
                    biased;
                    _ = wait_flag(&mut shutdown) => { merge.lock().await.clear_hid(); return; }
                    _ = tokio::time::sleep(RESOLVE_INTERVAL) => {}
                }
            }
        }
    }
}

/// The evdev node as a [`HidDevice`]. `evdev::Device` resyncs its state on a
/// dropped-event report itself, so a resync the kernel refuses surfaces as a
/// read error here.
#[cfg(target_os = "linux")]
impl HidDevice for evdev::EventStream {
    async fn next_input(&mut self) -> std::io::Result<HidInput> {
        use evdev::InputEventKind;
        let ev = self.next_event().await?;
        Ok(match ev.kind() {
            InputEventKind::AbsAxis(axis) => HidInput::Abs {
                code: axis.0,
                value: ev.value(),
            },
            InputEventKind::Key(key) => HidInput::Key {
                code: key.code(),
                pressed: ev.value() != 0,
            },
            _ => HidInput::Other,
        })
    }

    fn poll_state(&self) -> std::io::Result<HidSnapshot> {
        let device = self.device();
        let abs = device.get_abs_state()?;
        let keys = device.get_key_state()?;
        let mut snapshot = HidSnapshot::default();
        for (slot, map) in snapshot.axes.iter_mut().zip(DEFAULT_AXIS_MAP.iter()) {
            *slot = abs[map.code as usize].value;
        }
        for (slot, map) in snapshot.buttons.iter_mut().zip(DEFAULT_BUTTON_MAP.iter()) {
            *slot = keys.contains(evdev::Key::new(map.code));
        }
        Ok(snapshot)
    }
}

/// Open one evdev node and stream it into the merge until the device dies,
/// the primary selection changes, or shutdown latches.
#[cfg(target_os = "linux")]
async fn stream_device(
    node: &str,
    bound_primary: &str,
    merge: &std::sync::Arc<tokio::sync::Mutex<crate::sources::SourceMerge>>,
    shutdown: &mut tokio::sync::watch::Receiver<bool>,
) -> ReaderExit {
    use evdev::{AbsoluteAxisType, Device};

    let Ok(device) = Device::open(node) else {
        tracing::warn!(node, "hid_device_open_failed");
        return ReaderExit::DeviceLost;
    };
    // Per-axis calibration from the device's absinfo, for the mapped axes the
    // device actually reports. An axis it lacks gets no calibration, so its
    // channel keeps the default bank value instead of reading a fake center.
    let cal: Vec<AxisCal> = match device.get_abs_state() {
        Ok(abs) => DEFAULT_AXIS_MAP
            .iter()
            .filter(|m| {
                device
                    .supported_absolute_axes()
                    .is_some_and(|axes| axes.contains(AbsoluteAxisType(m.code)))
            })
            .map(|m| {
                let info = abs[m.code as usize];
                AxisCal {
                    code: m.code,
                    min: info.minimum,
                    max: info.maximum,
                }
            })
            .collect(),
        Err(e) => {
            tracing::warn!(node, error = %e, "hid_absinfo_read_failed");
            return ReaderExit::DeviceLost;
        }
    };
    let Ok(mut stream) = device.into_event_stream() else {
        tracing::warn!(node, "hid_event_stream_failed");
        return ReaderExit::DeviceLost;
    };
    tracing::info!(node, "hid_source_reading");

    // Ends the session on shutdown, or when the operator re-selects the
    // primary so the reader reopens onto it.
    let stop = async {
        let mut resolve_tick = tokio::time::interval(RESOLVE_INTERVAL);
        resolve_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                _ = wait_flag(shutdown) => return ReaderExit::Shutdown,
                _ = resolve_tick.tick() => {
                    let current = query_primary(&hid_cmd_sock_path()).await;
                    if current.as_deref() != Some(bound_primary) {
                        tracing::info!(node, "hid_primary_changed");
                        return ReaderExit::DeviceLost;
                    }
                }
            }
        }
    };
    drive_device(&mut stream, &cal, merge, stop).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::{CHANNEL_MAX, CHANNEL_MIN};

    // ── scaling ─────────────────────────────────────────────────────────────

    #[test]
    fn abs_scaling_hits_the_endpoints_and_center() {
        // A symmetric signed range: exact endpoints + exact center.
        assert_eq!(abs_to_channel(-32768, -32768, 32767, false), CHANNEL_MIN);
        assert_eq!(abs_to_channel(32767, -32768, 32767, false), CHANNEL_MAX);
        assert_eq!(abs_to_channel(0, -32768, 32767, false), CHANNEL_MID);
        // An unsigned 8-bit pad range.
        assert_eq!(abs_to_channel(0, 0, 255, false), CHANNEL_MIN);
        assert_eq!(abs_to_channel(255, 0, 255, false), CHANNEL_MAX);
        // 0..255 has no exact integer center; 128 lands within a step of it.
        let near_mid = abs_to_channel(128, 0, 255, false);
        assert!(
            (CHANNEL_MID..=CHANNEL_MID + 8).contains(&near_mid),
            "got {near_mid}"
        );
    }

    #[test]
    fn abs_scaling_inverts_and_clamps() {
        assert_eq!(abs_to_channel(-32768, -32768, 32767, true), CHANNEL_MAX);
        assert_eq!(abs_to_channel(32767, -32768, 32767, true), CHANNEL_MIN);
        // Out-of-range raw samples clamp to the device range first.
        assert_eq!(abs_to_channel(i32::MIN, -32768, 32767, false), CHANNEL_MIN);
        assert_eq!(abs_to_channel(i32::MAX, 0, 255, false), CHANNEL_MAX);
        // A degenerate range reads center rather than dividing by zero.
        assert_eq!(abs_to_channel(7, 5, 5, false), CHANNEL_MID);
    }

    #[test]
    fn abs_scaling_is_monotonic_over_a_device_range() {
        let mut prev = abs_to_channel(0, 0, 1023, false);
        for raw in (0..=1023).step_by(13) {
            let ch = abs_to_channel(raw, 0, 1023, false);
            assert!((CHANNEL_MIN..=CHANNEL_MAX).contains(&ch));
            assert!(ch >= prev, "monotonic");
            prev = ch;
        }
    }

    // ── the event → frame mapping ───────────────────────────────────────────

    fn full_cal() -> Vec<AxisCal> {
        DEFAULT_AXIS_MAP
            .iter()
            .map(|m| AxisCal {
                code: m.code,
                min: -32768,
                max: 32767,
            })
            .collect()
    }

    #[test]
    fn stick_axes_land_on_their_aetr_channels() {
        let mut frame = HidChannels::default();
        let cal = full_cal();
        // Right stick X full right → roll full high.
        assert!(frame.apply_abs(0x03, 32767, &cal));
        assert_eq!(frame.values()[0], CHANNEL_MAX);
        // Right stick Y full forward (negative) → pitch high (inverted).
        assert!(frame.apply_abs(0x04, -32768, &cal));
        assert_eq!(frame.values()[1], CHANNEL_MAX);
        // Left stick Y full up (negative) → throttle high (inverted).
        assert!(frame.apply_abs(0x01, -32768, &cal));
        assert_eq!(frame.values()[2], CHANNEL_MAX);
        // Left stick X centered → yaw center; no change from neutral.
        assert!(!frame.apply_abs(0x00, 0, &cal));
        assert_eq!(frame.values()[3], CHANNEL_MID);
        // An unmapped axis changes nothing.
        assert!(!frame.apply_abs(0x22, 100, &cal));
    }

    #[test]
    fn buttons_drive_two_position_aux_channels() {
        let mut frame = HidChannels::default();
        // BTN_SOUTH press → aux ch4 high; release → low.
        assert!(frame.apply_key(0x130, true));
        assert_eq!(frame.values()[4], CHANNEL_MAX);
        assert!(frame.apply_key(0x130, false));
        assert_eq!(frame.values()[4], CHANNEL_MIN);
        // A repeat of the same state is not a change.
        assert!(!frame.apply_key(0x130, false));
        // An unmapped key changes nothing.
        assert!(!frame.apply_key(0x1FF, true));
        // BTN_START lands on the last mapped aux channel.
        assert!(frame.apply_key(0x13B, true));
        assert_eq!(frame.values()[11], CHANNEL_MAX);
    }

    #[test]
    fn every_mapped_channel_is_distinct_and_in_range() {
        let mut seen = std::collections::HashSet::new();
        for m in DEFAULT_AXIS_MAP {
            assert!(m.channel < CHANNEL_COUNT);
            assert!(seen.insert(m.channel), "duplicate channel {}", m.channel);
        }
        for m in DEFAULT_BUTTON_MAP {
            assert!(m.channel < CHANNEL_COUNT);
            assert!(seen.insert(m.channel), "duplicate channel {}", m.channel);
        }
    }

    // ── device id resolution ────────────────────────────────────────────────

    #[test]
    fn device_path_resolves_the_event_basename() {
        assert_eq!(
            device_path_from_id("usb:045e:028e:event3").as_deref(),
            Some("/dev/input/event3")
        );
        assert_eq!(
            device_path_from_id("usb:045e:028e:event17").as_deref(),
            Some("/dev/input/event17")
        );
        // Not an evdev basename → no path (never a guessed node).
        assert!(device_path_from_id("usb:045e:028e:js0").is_none());
        assert!(device_path_from_id("event").is_none());
        assert!(device_path_from_id("usb:045e:028e:eventX").is_none());
        assert!(device_path_from_id("").is_none());
    }

    // ── the get_primary round-trip ──────────────────────────────────────────

    async fn fake_hid_socket(dir: &Path, canned: Value) -> PathBuf {
        use tokio::net::UnixListener;
        let sock = dir.join("hid-cmd.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let mut body = serde_json::to_vec(&canned).unwrap();
                body.push(b'\n');
                let _ = stream.write_all(&body).await;
                let _ = stream.flush().await;
            }
        });
        sock
    }

    #[tokio::test]
    async fn query_primary_reads_the_bound_id() {
        let dir = tempfile::tempdir().unwrap();
        let sock = fake_hid_socket(
            dir.path(),
            json!({"ok": true, "primary_id": "usb:045e:028e:event3"}),
        )
        .await;
        assert_eq!(
            query_primary(&sock).await.as_deref(),
            Some("usb:045e:028e:event3")
        );
    }

    #[tokio::test]
    async fn query_primary_is_none_when_unbound_or_unreachable() {
        let dir = tempfile::tempdir().unwrap();
        let sock = fake_hid_socket(dir.path(), json!({"ok": true, "primary_id": null})).await;
        assert!(query_primary(&sock).await.is_none());
        // No listener at all.
        let gone = dir.path().join("nope.sock");
        assert!(query_primary(&gone).await.is_none());
    }

    // ── device liveness ─────────────────────────────────────────────────────

    /// A device that never emits an event (a stick held perfectly still) and
    /// whose state poll can be made to fail, as an unplugged node does.
    struct StillDevice {
        snapshot: HidSnapshot,
        poll_fails: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl HidDevice for StillDevice {
        async fn next_input(&mut self) -> std::io::Result<HidInput> {
            std::future::pending().await
        }

        fn poll_state(&self) -> std::io::Result<HidSnapshot> {
            if self.poll_fails.load(std::sync::atomic::Ordering::SeqCst) {
                Err(std::io::Error::from_raw_os_error(19))
            } else {
                Ok(self.snapshot)
            }
        }
    }

    /// Throttle stick held full up, every other axis centered.
    fn throttle_up_snapshot() -> HidSnapshot {
        let mut snapshot = HidSnapshot::default();
        for (slot, map) in snapshot.axes.iter_mut().zip(DEFAULT_AXIS_MAP.iter()) {
            if map.channel == 2 {
                *slot = -32768;
            }
        }
        snapshot
    }

    fn hid_merge() -> std::sync::Arc<tokio::sync::Mutex<crate::sources::SourceMerge>> {
        std::sync::Arc::new(tokio::sync::Mutex::new(crate::sources::SourceMerge::new(
            crate::sources::ChannelSourceMode::Hid,
        )))
    }

    #[tokio::test]
    async fn a_device_held_still_keeps_its_stick_past_the_stale_window() {
        use crate::sources::{ChannelSource, HID_STALE_AFTER};

        let merge = hid_merge();
        let device = StillDevice {
            snapshot: throttle_up_snapshot(),
            poll_fails: Default::default(),
        };
        let reader_merge = merge.clone();
        let reader = tokio::spawn(async move {
            let mut device = device;
            drive_device(
                &mut device,
                &full_cal(),
                &reader_merge,
                std::future::pending(),
            )
            .await
        });

        // Well past the stale window with no event at all: the successful
        // polls keep the slot live, and the frame came from the polled state
        // (seeded at open), not the throttle-low default.
        tokio::time::sleep(HID_STALE_AFTER + std::time::Duration::from_millis(300)).await;
        let (values, source) = merge
            .lock()
            .await
            .current(std::time::Instant::now())
            .expect("a still but answering device stays live");
        assert_eq!(source, ChannelSource::Hid);
        assert_eq!(
            values[2], CHANNEL_MAX,
            "throttle held where the pilot left it"
        );
        reader.abort();
    }

    #[tokio::test]
    async fn a_device_that_stops_answering_goes_stale_and_rc_frames_stop() {
        use crate::sources::HID_STALE_AFTER;
        use crate::transport::{run_tx, OobQueue, WireCounters};
        use std::sync::atomic::Ordering;
        use std::time::{Duration, Instant};

        let merge = hid_merge();
        let poll_fails = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let device = StillDevice {
            snapshot: throttle_up_snapshot(),
            poll_fails: poll_fails.clone(),
        };
        let reader_merge = merge.clone();
        let reader = tokio::spawn(async move {
            let mut device = device;
            drive_device(
                &mut device,
                &full_cal(),
                &reader_merge,
                std::future::pending(),
            )
            .await
        });
        let (tx_side, _rx_keep) = tokio::io::duplex(1 << 16);
        let counters = std::sync::Arc::new(WireCounters::default());
        let cancel = ados_protocol::shutdown::Shutdown::new();
        let tx = tokio::spawn(run_tx(
            tx_side,
            merge.clone(),
            200,
            counters.clone(),
            std::sync::Arc::new(OobQueue::default()),
            cancel.clone(),
        ));

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            counters.tx_frames.load(Ordering::Relaxed) > 0,
            "frames flow while the device answers"
        );

        // The device stops answering its state poll.
        let failed_at = Instant::now();
        poll_fails.store(true, Ordering::SeqCst);
        let exit = tokio::time::timeout(HID_STALE_AFTER, reader)
            .await
            .expect("dropped inside the stale window")
            .unwrap();
        assert_eq!(exit, ReaderExit::DeviceLost);
        assert!(failed_at.elapsed() < HID_STALE_AFTER);
        assert!(merge.lock().await.current(Instant::now()).is_none());

        // One frame may already have been in flight; after that the wire is
        // silent, so the receiver's failsafe runs.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let settled = counters.tx_frames.load(Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(counters.tx_frames.load(Ordering::Relaxed), settled);

        cancel.trigger();
        tx.await.unwrap();
    }
}
