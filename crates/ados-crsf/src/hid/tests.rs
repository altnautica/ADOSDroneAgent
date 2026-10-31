use super::*;
use ados_crsf_codec::channels::{CHANNEL_MAX, CHANNEL_MIN};

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
