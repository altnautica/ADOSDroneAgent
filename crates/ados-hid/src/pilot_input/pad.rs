//! Raw evdev pad state onto spec controls: driver detection, normalisation,
//! stick conditioning (radial deadzone, stick mode) and the `buttons` bitfield.
//!
//! Pure and host-tested; the Linux reader fills a [`RawPad`] from the kernel.

use std::collections::{BTreeMap, BTreeSet};

use super::spec::{ds4, evdev_code, EvdevMap, StickMode};

/// The kernel driver behind the pad, which decides the axis codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Driver {
    /// `hid-playstation` (Linux 6.2+), and `hid-sony` builds that expose the
    /// same standard map: right stick on ABS_RX/RY, triggers on ABS_Z/RZ.
    HidPlaystation,
    /// Older `hid-sony`: no ABS_RX; right stick on ABS_Z/RZ, triggers on ABS_HAT2*.
    HidSonyLegacy,
}

impl Driver {
    /// Detection per the spec: a pad without ABS_RX is the legacy map.
    pub fn detect(has_abs_rx: bool) -> Self {
        if has_abs_rx {
            Self::HidPlaystation
        } else {
            Self::HidSonyLegacy
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::HidPlaystation => "hid-playstation",
            Self::HidSonyLegacy => "hid-sony-legacy",
        }
    }
}

/// One absolute axis as the kernel reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AbsAxis {
    pub value: i32,
    pub min: i32,
    pub max: i32,
}

/// One kernel poll of the pad: every absolute axis and the keys held down.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawPad {
    pub abs: BTreeMap<u16, AbsAxis>,
    pub keys: BTreeSet<u16>,
}

/// The pad's controls, normalised. Stick axes run -1..1 with evdev's sign
/// (y grows downward); triggers 0..1; `buttons` is the spec bitfield.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PadState {
    pub stick_l: (f32, f32),
    pub stick_r: (f32, f32),
    pub l2: f32,
    pub r2: f32,
    pub buttons: u32,
}

impl PadState {
    /// Whether the control's bit is set.
    pub fn pressed(&self, control: &str) -> bool {
        bit(control).is_some_and(|b| self.buttons & (1 << b) != 0)
    }
}

/// The spec bit of a control id (`cross`, `ps`, `dpad_up`, …).
pub fn bit(control: &str) -> Option<u32> {
    ds4().stream.buttons_bitfield.get(control).copied()
}

/// The resolved code map of one driver.
#[derive(Debug, Clone)]
pub struct PadMap {
    axes: BTreeMap<String, u16>,
    buttons: BTreeMap<String, u16>,
}

impl PadMap {
    /// The map for `driver`. The legacy map lists only the axes that moved;
    /// everything else (buttons, D-pad hat) falls back to the standard map.
    pub fn for_driver(driver: Driver) -> Self {
        let linux = &ds4().platform_map.linux_evdev;
        let resolve = |m: &EvdevMap| -> (BTreeMap<String, u16>, BTreeMap<String, u16>) {
            let conv = |src: &BTreeMap<String, String>| {
                src.iter()
                    .filter_map(|(k, v)| evdev_code(v).map(|c| (k.clone(), c)))
                    .collect::<BTreeMap<_, _>>()
            };
            (conv(&m.axes), conv(&m.buttons))
        };
        let (mut axes, mut buttons) = resolve(&linux.hid_playstation);
        if driver == Driver::HidSonyLegacy {
            let (la, lb) = resolve(&linux.hid_sony_legacy);
            axes.extend(la);
            buttons.extend(lb);
        }
        Self { axes, buttons }
    }

    fn axis(&self, raw: &RawPad, name: &str) -> Option<AbsAxis> {
        self.axes.get(name).and_then(|c| raw.abs.get(c)).copied()
    }

    /// Normalise one poll.
    pub fn read(&self, raw: &RawPad, trigger_threshold: f32) -> PadState {
        let stick = |n: &str| self.axis(raw, n).map(centred).unwrap_or(0.0);
        let trigger = |n: &str| self.axis(raw, n).map(unit).unwrap_or(0.0);
        let mut st = PadState {
            stick_l: (stick("stick_l_x"), stick("stick_l_y")),
            stick_r: (stick("stick_r_x"), stick("stick_r_y")),
            l2: trigger("l2"),
            r2: trigger("r2"),
            buttons: 0,
        };
        let mut set = |control: &str| {
            if let Some(b) = bit(control) {
                st.buttons |= 1 << b;
            }
        };
        for (control, code) in &self.buttons {
            if raw.keys.contains(code) {
                set(control);
            }
        }
        if st.l2 > trigger_threshold {
            set("l2");
        }
        if st.r2 > trigger_threshold {
            set("r2");
        }
        let hat = |n: &str| self.axis(raw, n).map(|a| a.value.signum()).unwrap_or(0);
        match hat("dpad_x") {
            -1 => set("dpad_left"),
            1 => set("dpad_right"),
            _ => {}
        }
        match hat("dpad_y") {
            -1 => set("dpad_up"),
            1 => set("dpad_down"),
            _ => {}
        }
        st
    }
}

/// A stick axis around its range midpoint onto -1..1.
fn centred(a: AbsAxis) -> f32 {
    if a.max <= a.min {
        return 0.0;
    }
    let mid = (f64::from(a.min) + f64::from(a.max)) / 2.0;
    let half = (f64::from(a.max) - f64::from(a.min)) / 2.0;
    (((f64::from(a.value) - mid) / half).clamp(-1.0, 1.0)) as f32
}

/// A trigger axis onto 0..1.
fn unit(a: AbsAxis) -> f32 {
    if a.max <= a.min {
        return 0.0;
    }
    (((f64::from(a.value) - f64::from(a.min)) / (f64::from(a.max) - f64::from(a.min)))
        .clamp(0.0, 1.0)) as f32
}

/// Radial deadzone with rescale: inside `dz` reads zero, the rim stays 1.
pub fn radial_deadzone((x, y): (f32, f32), dz: f32) -> (f32, f32) {
    let mag = (x * x + y * y).sqrt();
    if mag <= dz || mag == 0.0 {
        return (0.0, 0.0);
    }
    let scaled = ((mag - dz) / (1.0 - dz)).min(1.0);
    let k = scaled / mag;
    ((x * k).clamp(-1.0, 1.0), (y * k).clamp(-1.0, 1.0))
}

/// The pilot `axes` `[roll, pitch, thr, yaw]` (C1 signs) from a pad state
/// through the stick mode and the deadzone.
pub fn pilot_axes(st: &PadState, mode: &StickMode, dz: f32) -> [f32; 4] {
    let l = radial_deadzone(st.stick_l, dz);
    let r = radial_deadzone(st.stick_r, dz);
    let pick = |src: &super::spec::AxisSource| {
        let s = if src.stick == "stick_l" { l } else { r };
        let v = if src.axis == "x" { s.0 } else { s.1 };
        if src.invert {
            -v
        } else {
            v
        }
    };
    [
        pick(&mode.roll),
        pick(&mode.pitch),
        pick(&mode.throttle),
        pick(&mode.yaw),
    ]
}

/// Whether a stick sits inside the spec's centred threshold on both axes.
pub fn is_centred((x, y): (f32, f32)) -> bool {
    let t = ds4().centered_threshold;
    x.abs() < t && y.abs() < t
}

#[cfg(test)]
mod tests;
