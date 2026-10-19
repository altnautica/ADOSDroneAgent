use std::collections::{HashMap, HashSet};

use super::*;

/// A fake V4L2 control surface: each control holds a range and a value. A
/// write is snapped down to the control's step, like a driver does.
#[derive(Default)]
struct FakeDevice {
    controls: HashMap<u32, (ControlInfo, i32)>,
    /// Controls whose writes the driver refuses.
    refuse: HashSet<u32>,
    /// Controls that accept a write and keep their old value.
    ignore: HashSet<u32>,
    /// Exposure modes the driver refuses.
    refuse_modes: HashSet<i32>,
}

fn info(minimum: i32, maximum: i32, step: i32) -> ControlInfo {
    ControlInfo {
        minimum,
        maximum,
        step,
    }
}

impl FakeDevice {
    /// Exposure (100 µs units, 1..=5000), auto mode (aperture priority) and
    /// analogue gain (0..=255).
    fn camera() -> Self {
        let mut d = Self::default();
        d.controls
            .insert(CID_EXPOSURE_ABSOLUTE, (info(1, 5000, 1), 50));
        d.controls.insert(CID_EXPOSURE_AUTO, (info(0, 3, 1), 3));
        d.controls.insert(CID_ANALOGUE_GAIN, (info(0, 255, 1), 16));
        d
    }

    fn value(&self, id: u32) -> i32 {
        self.controls[&id].1
    }
}

impl ControlDevice for FakeDevice {
    fn query(&mut self, id: u32) -> Option<ControlInfo> {
        self.controls.get(&id).map(|(info, _)| *info)
    }

    fn set(&mut self, id: u32, value: i32) -> io::Result<()> {
        let refused = self.refuse.contains(&id)
            || (id == CID_EXPOSURE_AUTO && self.refuse_modes.contains(&value));
        let Some((info, current)) = self.controls.get_mut(&id).filter(|_| !refused) else {
            return Err(io::Error::from_raw_os_error(22));
        };
        if !self.ignore.contains(&id) {
            *current = value - (value - info.minimum) % info.step.max(1);
        }
        Ok(())
    }

    fn get(&mut self, id: u32) -> io::Result<i32> {
        self.controls
            .get(&id)
            .map(|(_, v)| *v)
            .ok_or_else(|| io::Error::from_raw_os_error(22))
    }
}

fn request(
    exposure_us: Option<u32>,
    gain: Option<f32>,
    auto: Option<bool>,
) -> CameraControlRequest {
    CameraControlRequest {
        camera_id: "cam".into(),
        exposure_us,
        gain,
        auto_exposure: auto,
    }
}

#[test]
fn manual_exposure_and_gain_are_set_and_read_back() {
    let mut dev = FakeDevice::camera();
    let r = apply(&mut dev, &request(Some(2_000), Some(8.0), None));
    assert_eq!(
        r,
        CameraControlResult {
            applied: true,
            exposure_us: Some(2_000),
            gain: Some(8.0),
            reason: None,
        }
    );
    // A manual exposure time switches the camera to manual mode first.
    assert_eq!(dev.value(CID_EXPOSURE_AUTO), MANUAL);
    assert_eq!(dev.value(CID_EXPOSURE_ABSOLUTE), 20);
}

#[test]
fn a_read_back_within_one_driver_step_is_applied_with_the_driver_value() {
    let mut dev = FakeDevice::camera();
    dev.controls
        .insert(CID_EXPOSURE_ABSOLUTE, (info(1, 5000, 4), 49));
    // A step of 4 from a minimum of 1: the driver snaps 23 units to 21,
    // within one step of the request.
    let r = apply(&mut dev, &request(Some(2_300), None, None));
    assert!(r.applied, "{r:?}");
    assert_eq!(r.exposure_us, Some(2_100));
    assert_eq!(r.gain, Some(16.0));
}

#[test]
fn a_read_back_that_differs_is_a_mismatch_with_the_actual_values() {
    let mut dev = FakeDevice::camera();
    dev.ignore.insert(CID_EXPOSURE_ABSOLUTE);
    let r = apply(&mut dev, &request(Some(2_000), None, None));
    assert!(!r.applied);
    assert_eq!(r.reason.as_deref(), Some(reason::READBACK_MISMATCH));
    assert_eq!(r.exposure_us, Some(5_000));

    let mut dev = FakeDevice::camera();
    dev.ignore.insert(CID_EXPOSURE_AUTO);
    let r = apply(&mut dev, &request(None, None, Some(false)));
    assert_eq!(r.reason.as_deref(), Some(reason::READBACK_MISMATCH));
}

#[test]
fn a_missing_or_refused_control_is_not_applied() {
    let mut dev = FakeDevice::camera();
    dev.controls.remove(&CID_ANALOGUE_GAIN);
    let r = apply(&mut dev, &request(Some(2_000), Some(4.0), None));
    assert_eq!(r.reason.as_deref(), Some(reason::CONTROL_UNSUPPORTED));
    assert_eq!(r.exposure_us, Some(5_000));
    assert_eq!(r.gain, None);
    // Nothing was written before the refusal.
    assert_eq!(dev.value(CID_EXPOSURE_AUTO), 3);

    let mut dev = FakeDevice::camera();
    dev.controls.remove(&CID_EXPOSURE_ABSOLUTE);
    let r = apply(&mut dev, &request(Some(2_000), None, None));
    assert_eq!(r.reason.as_deref(), Some(reason::CONTROL_UNSUPPORTED));

    let mut dev = FakeDevice::camera();
    dev.controls.remove(&CID_EXPOSURE_AUTO);
    let r = apply(&mut dev, &request(None, None, Some(true)));
    assert_eq!(r.reason.as_deref(), Some(reason::CONTROL_UNSUPPORTED));

    let mut dev = FakeDevice::camera();
    dev.refuse.insert(CID_ANALOGUE_GAIN);
    let r = apply(&mut dev, &request(None, Some(4.0), None));
    assert_eq!(r.reason.as_deref(), Some(reason::SET_FAILED));
}

#[test]
fn gain_falls_back_to_the_older_control() {
    let mut dev = FakeDevice::camera();
    dev.controls.remove(&CID_ANALOGUE_GAIN);
    dev.controls.insert(CID_GAIN, (info(0, 100, 1), 0));
    let r = apply(&mut dev, &request(None, Some(3.4), None));
    assert!(r.applied, "{r:?}");
    assert_eq!(r.gain, Some(3.0));
    assert_eq!(dev.value(CID_GAIN), 3);
}

#[test]
fn auto_exposure_takes_the_first_mode_the_driver_accepts() {
    let mut dev = FakeDevice::camera();
    dev.controls
        .insert(CID_EXPOSURE_AUTO, (info(0, 3, 1), MANUAL));
    dev.refuse_modes.insert(0);
    let r = apply(&mut dev, &request(None, None, Some(true)));
    assert!(r.applied, "{r:?}");
    assert_eq!(dev.value(CID_EXPOSURE_AUTO), 3);

    let r = apply(&mut dev, &request(None, None, Some(false)));
    assert!(r.applied, "{r:?}");
    assert_eq!(dev.value(CID_EXPOSURE_AUTO), MANUAL);
}

#[test]
fn malformed_requests_change_nothing() {
    for (req, why) in [
        (
            request(Some(1_000), None, Some(true)),
            reason::CONFLICTING_REQUEST,
        ),
        (request(None, Some(f32::NAN), None), reason::INVALID_VALUE),
        (request(None, Some(-1.0), None), reason::INVALID_VALUE),
        (request(Some(900_000), None, None), reason::OUT_OF_RANGE),
        (request(None, Some(1e12), None), reason::OUT_OF_RANGE),
    ] {
        let mut dev = FakeDevice::camera();
        let r = apply(&mut dev, &req);
        assert!(!r.applied);
        assert_eq!(r.reason.as_deref(), Some(why), "{req:?}");
        assert_eq!(dev.value(CID_EXPOSURE_ABSOLUTE), 50);
        assert_eq!(dev.value(CID_EXPOSURE_AUTO), 3);
        assert_eq!(dev.value(CID_ANALOGUE_GAIN), 16);
    }
}

#[test]
fn an_empty_request_reports_the_current_values() {
    let mut dev = FakeDevice::camera();
    let r = apply(&mut dev, &request(None, None, None));
    assert_eq!(
        r,
        CameraControlResult {
            applied: false,
            exposure_us: Some(5_000),
            gain: Some(16.0),
            reason: Some(reason::EMPTY_REQUEST.into()),
        }
    );
}

#[test]
fn drain_answers_every_queued_call() {
    let (tx, mut rx) = mpsc::channel(4);
    let mut replies = Vec::new();
    for us in [1_000, 3_000] {
        let (reply, answer) = oneshot::channel();
        tx.try_send(ControlCall {
            request: request(Some(us), None, None),
            reply,
        })
        .unwrap();
        replies.push(answer);
    }
    let mut dev = FakeDevice::camera();
    assert!(drain(&mut rx, &mut dev));
    let got: Vec<_> = replies
        .into_iter()
        .map(|mut a| a.try_recv().unwrap().exposure_us)
        .collect();
    assert_eq!(got, [Some(1_000), Some(3_000)]);
    assert!(!drain(&mut rx, &mut dev));
}
