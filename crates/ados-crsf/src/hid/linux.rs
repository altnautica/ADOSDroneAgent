//! The Linux evdev reader.

use super::*;

/// How often the reader re-resolves the primary gamepad (and notices a
/// selection change) while idle or while a device is open.
#[cfg(target_os = "linux")]
pub(super) const RESOLVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Await the latched shutdown flag, dropping the watch guard inside so the
/// wrapping future stays `Send` (the raw `wait_for` future's output holds a
/// lock guard the spawned task must not carry).
#[cfg(target_os = "linux")]
pub(super) async fn wait_flag(rx: &mut tokio::sync::watch::Receiver<bool>) {
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
pub(super) async fn stream_device(
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
