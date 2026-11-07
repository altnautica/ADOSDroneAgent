//! STATUSTEXT progress parsers for AUTOTUNE (Copter, QuadPlane, Plane, PX4).
//! PX4's `[cal]` protocol lives in `adapters::px4`.

/// Events an autotune reports through STATUSTEXT.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuneEvent {
    Started,
    /// Copter/QuadPlane: an axis/step is under test (`AutoTune: Testing`, …).
    Phase(String),
    /// Copter/QuadPlane: every axis in `AUTOTUNE_AXES` finished.
    Success,
    Failed,
    /// `AutoTune: Saved gains for Roll Pitch` (the axes listed).
    Saved(Vec<Axis>),
    /// Plane: `Roll: Finished`.
    AxisDone(Axis),
    /// The pilot's sticks paused the tune.
    PilotOverride,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Axis {
    Roll,
    Pitch,
    Yaw,
}

fn axes_in(s: &str) -> Vec<Axis> {
    let l = s.to_ascii_lowercase();
    let mut v = Vec::new();
    if l.contains("roll") {
        v.push(Axis::Roll);
    }
    if l.contains("pitch") {
        v.push(Axis::Pitch);
    }
    if l.contains("yaw") {
        v.push(Axis::Yaw);
    }
    v
}

/// Parses ArduPilot `AC_AutoTune` (Copter, QAUTOTUNE) and `AP_AutoTune`
/// (Plane) messages, and PX4 autotune messages.
pub fn parse_tune(text: &str) -> Option<TuneEvent> {
    let t = text.trim();
    // Plane AP_AutoTune: "<Axis>: Finished".
    if let Some(axis) = t.strip_suffix(": Finished") {
        return axes_in(axis).first().copied().map(TuneEvent::AxisDone);
    }
    let lower = t.to_ascii_lowercase();
    let rest = match lower.strip_prefix("autotune: ") {
        Some(r) => r,
        None => lower.strip_prefix("autotune ")?,
    };
    Some(if rest.starts_with("started") {
        TuneEvent::Started
    } else if rest.starts_with("success") || rest.starts_with("completed") || rest.starts_with("done") {
        TuneEvent::Success
    } else if rest.starts_with("failed") {
        TuneEvent::Failed
    } else if rest.starts_with("saved gains for") {
        TuneEvent::Saved(axes_in(rest))
    } else if rest.contains("pilot override") {
        TuneEvent::PilotOverride
    } else {
        TuneEvent::Phase(t["AutoTune: ".len().min(t.len())..].to_string())
    })
}

/// A PreArm / Arm text that says the FC needs calibration.
pub fn needs_calibration(text: &str) -> bool {
    let l = text.to_ascii_lowercase();
    (l.starts_with("prearm:") || l.starts_with("arm:"))
        && (l.contains("calibration needed")
            || l.contains("not calibrated")
            || l.contains("compass")
            || l.contains("accel")
            || l.contains("gyro"))
}
