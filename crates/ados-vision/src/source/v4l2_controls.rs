//! Exposure, gain and auto-exposure on a V4L2 camera this engine streams.
//!
//! A plugin's control request reaches the capture thread as a [`ControlCall`]
//! and is applied through the device handle the thread already streams from,
//! so the device is never opened a second time. [`apply`] sets the requested
//! controls and reads every touched one back; it is written against
//! [`ControlDevice`] so the decision logic runs without a kernel.

use std::io;

use ados_protocol::camera_control::{reason, CameraControlRequest, CameraControlResult};
use tokio::sync::{mpsc, oneshot};

use super::{
    CID_ANALOGUE_GAIN, CID_EXPOSURE_ABSOLUTE, CID_EXPOSURE_AUTO, CID_GAIN, EXPOSURE_MANUAL,
};

/// `V4L2_EXPOSURE_AUTO`, then `V4L2_EXPOSURE_APERTURE_PRIORITY`: the automatic
/// modes tried in order (UVC cameras commonly offer only the second).
const AUTO_MODES: [i32; 2] = [0, 3];
const MANUAL: i32 = EXPOSURE_MANUAL as i32;
/// `V4L2_CID_EXPOSURE_ABSOLUTE` counts in 100 µs.
const EXPOSURE_UNIT_US: u32 = 100;

/// One control request handed to the capture thread, with where its result
/// goes.
#[derive(Debug)]
pub struct ControlCall {
    pub request: CameraControlRequest,
    pub reply: oneshot::Sender<CameraControlResult>,
}

/// The range of an integer control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlInfo {
    pub minimum: i32,
    pub maximum: i32,
    pub step: i32,
}

/// A camera's control surface.
pub trait ControlDevice {
    /// The control's range; `None` when the driver lacks the control or
    /// reports it disabled or read-only.
    fn query(&mut self, id: u32) -> Option<ControlInfo>;
    fn set(&mut self, id: u32, value: i32) -> io::Result<()>;
    fn get(&mut self, id: u32) -> io::Result<i32>;
}

/// Apply every queued call on `dev`. True when at least one was applied, so
/// the caller re-reads the exposure it stamps frames with.
pub fn drain(rx: &mut mpsc::Receiver<ControlCall>, dev: &mut impl ControlDevice) -> bool {
    let mut any = false;
    while let Ok(call) = rx.try_recv() {
        let result = apply(dev, &call.request);
        // The requester may have timed out; the controls are set regardless.
        let _ = call.reply.send(result);
        any = true;
    }
    any
}

/// The controls present on one device.
struct Present {
    exposure: Option<ControlInfo>,
    auto: Option<ControlInfo>,
    gain: Option<(u32, ControlInfo)>,
}

impl Present {
    fn probe(dev: &mut impl ControlDevice) -> Self {
        Self {
            exposure: dev.query(CID_EXPOSURE_ABSOLUTE),
            auto: dev.query(CID_EXPOSURE_AUTO),
            gain: [CID_ANALOGUE_GAIN, CID_GAIN]
                .into_iter()
                .find_map(|id| dev.query(id).map(|info| (id, info))),
        }
    }
}

/// The exposure in µs and the gain the device reports now.
fn read_back(dev: &mut impl ControlDevice, present: &Present) -> (Option<u32>, Option<f32>) {
    let exposure = present
        .exposure
        .and_then(|_| dev.get(CID_EXPOSURE_ABSOLUTE).ok())
        .and_then(|v| u32::try_from(v).ok())
        .and_then(|v| v.checked_mul(EXPOSURE_UNIT_US));
    let gain = present
        .gain
        .and_then(|(id, _)| dev.get(id).ok())
        .map(|v| v as f32);
    (exposure, gain)
}

fn outcome(
    dev: &mut impl ControlDevice,
    present: &Present,
    applied: bool,
    why: Option<&str>,
) -> CameraControlResult {
    let (exposure_us, gain) = read_back(dev, present);
    CameraControlResult {
        applied,
        exposure_us,
        gain,
        reason: why.map(str::to_string),
    }
}

/// A value read back within one driver step of the one set.
fn within_step(dev: &mut impl ControlDevice, id: u32, want: i32, info: ControlInfo) -> bool {
    dev.get(id)
        .is_ok_and(|got| (i64::from(got) - i64::from(want)).abs() <= i64::from(info.step.max(1)))
}

fn in_range(value: i32, info: ControlInfo) -> bool {
    (info.minimum..=info.maximum).contains(&value)
}

/// Set the requested controls on `dev` and read them back. `applied` only when
/// every requested control was set and reads back as asked (exposure and gain
/// within one driver step); the returned values are the read-back ones.
pub fn apply(dev: &mut impl ControlDevice, req: &CameraControlRequest) -> CameraControlResult {
    let present = Present::probe(dev);
    if req.exposure_us.is_none() && req.gain.is_none() && req.auto_exposure.is_none() {
        return outcome(dev, &present, false, Some(reason::EMPTY_REQUEST));
    }
    if req.auto_exposure == Some(true) && req.exposure_us.is_some() {
        return outcome(dev, &present, false, Some(reason::CONFLICTING_REQUEST));
    }
    if req.gain.is_some_and(|g| !g.is_finite() || g < 0.0) {
        return outcome(dev, &present, false, Some(reason::INVALID_VALUE));
    }
    let exposure = match (req.exposure_us, present.exposure) {
        (None, _) => None,
        (Some(_), None) => return outcome(dev, &present, false, Some(reason::CONTROL_UNSUPPORTED)),
        (Some(us), Some(info)) => {
            let units =
                (u64::from(us) + u64::from(EXPOSURE_UNIT_US / 2)) / u64::from(EXPOSURE_UNIT_US);
            Some((i32::try_from(units.max(1)).unwrap_or(i32::MAX), info))
        }
    };
    let gain = match (req.gain, present.gain) {
        (None, _) => None,
        (Some(_), None) => return outcome(dev, &present, false, Some(reason::CONTROL_UNSUPPORTED)),
        // `as` saturates; the range check below refuses a saturated value.
        (Some(g), Some((id, info))) => Some((id, g.round() as i32, info)),
    };
    if req.auto_exposure.is_some() && present.auto.is_none() {
        return outcome(dev, &present, false, Some(reason::CONTROL_UNSUPPORTED));
    }
    let ranged = exposure.is_none_or(|(v, info)| in_range(v, info))
        && gain.is_none_or(|(_, v, info)| in_range(v, info));
    if !ranged {
        return outcome(dev, &present, false, Some(reason::OUT_OF_RANGE));
    }

    // The mode first: a driver ignores or refuses an exposure time while
    // automatic exposure owns it. A manual exposure time implies manual mode.
    let want_auto = match req.auto_exposure {
        Some(auto) => Some(auto),
        None if exposure.is_some() && present.auto.is_some() => Some(false),
        None => None,
    };
    let mut set_ok = match want_auto {
        Some(true) => AUTO_MODES
            .iter()
            .any(|&mode| dev.set(CID_EXPOSURE_AUTO, mode).is_ok()),
        Some(false) => dev.set(CID_EXPOSURE_AUTO, MANUAL).is_ok(),
        None => true,
    };
    if let (true, Some((v, _))) = (set_ok, exposure) {
        set_ok = dev.set(CID_EXPOSURE_ABSOLUTE, v).is_ok();
    }
    if let (true, Some((id, v, _))) = (set_ok, gain) {
        set_ok = dev.set(id, v).is_ok();
    }
    if !set_ok {
        return outcome(dev, &present, false, Some(reason::SET_FAILED));
    }

    let mode_ok = want_auto.is_none_or(|auto| {
        dev.get(CID_EXPOSURE_AUTO)
            .is_ok_and(|mode| (mode != MANUAL) == auto)
    });
    let exposure_ok =
        exposure.is_none_or(|(v, info)| within_step(dev, CID_EXPOSURE_ABSOLUTE, v, info));
    let gain_ok = gain.is_none_or(|(id, v, info)| within_step(dev, id, v, info));
    if mode_ok && exposure_ok && gain_ok {
        outcome(dev, &present, true, None)
    } else {
        outcome(dev, &present, false, Some(reason::READBACK_MISMATCH))
    }
}

#[cfg(test)]
#[path = "v4l2_controls_tests.rs"]
mod tests;
