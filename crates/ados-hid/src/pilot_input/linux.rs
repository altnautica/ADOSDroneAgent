//! The Linux pad: evdev state polls, the lightbar through the multicolour LED
//! class device, the battery through the power-supply class, and FF_RUMBLE.

use std::path::{Path, PathBuf};

use evdev::{
    AbsoluteAxisType, Device, FFEffectData, FFEffectKind, FFEffectType, FFReplay, FFTrigger,
};

use super::feedback::Pulse;
use super::pad::{AbsAxis, Driver, RawPad};
use super::status::Controller;

/// One open pad.
pub struct OpenPad {
    dev: Device,
    pub driver: Driver,
    pub id: String,
    name: String,
    bluetooth: bool,
    hid_dir: Option<PathBuf>,
    led: Option<PathBuf>,
    led_max: u32,
    rumble: bool,
    /// The effect playing now; dropping it erases it from the device.
    playing: Option<evdev::FFEffect>,
    last_rgb: Option<[u8; 3]>,
}

/// The HID device directory behind `/dev/input/eventN`
/// (`/sys/class/input/eventN/device/device`), where `leds/` and `power_supply/` live.
fn hid_dir(node: &Path) -> Option<PathBuf> {
    let base = node.file_name()?;
    std::fs::canonicalize(
        Path::new("/sys/class/input")
            .join(base)
            .join("device/device"),
    )
    .ok()
}

fn find_led(hid: &Path) -> Option<PathBuf> {
    std::fs::read_dir(hid.join("leds"))
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(":rgb:indicator"))
        })
}

fn read_u32(p: &Path) -> Option<u32> {
    std::fs::read_to_string(p).ok()?.trim().parse().ok()
}

impl OpenPad {
    pub fn open(id: &str, node: &Path) -> std::io::Result<Self> {
        let dev = Device::open(node)?;
        let has_rx = dev
            .supported_absolute_axes()
            .is_some_and(|s| s.contains(AbsoluteAxisType::ABS_RX));
        let rumble = dev
            .supported_ff()
            .is_some_and(|s| s.contains(FFEffectType::FF_RUMBLE));
        let bluetooth = dev.input_id().bus_type() == evdev::BusType::BUS_BLUETOOTH;
        let hid_dir = hid_dir(node);
        let led = hid_dir.as_deref().and_then(find_led);
        let led_max = led
            .as_deref()
            .and_then(|l| read_u32(&l.join("max_brightness")))
            .unwrap_or(255);
        Ok(Self {
            name: dev.name().unwrap_or("unknown").to_string(),
            driver: Driver::detect(has_rx),
            id: id.to_string(),
            bluetooth,
            hid_dir,
            led,
            led_max,
            rumble,
            playing: None,
            last_rgb: None,
            dev,
        })
    }

    /// The pad's current state straight from the kernel.
    pub fn poll(&self) -> std::io::Result<RawPad> {
        let abs = self.dev.get_abs_state()?;
        let keys = self.dev.get_key_state()?;
        let mut raw = RawPad::default();
        if let Some(supported) = self.dev.supported_absolute_axes() {
            for axis in supported.iter() {
                let info = abs[axis.0 as usize];
                raw.abs.insert(
                    axis.0,
                    AbsAxis {
                        value: info.value,
                        min: info.minimum,
                        max: info.maximum,
                    },
                );
            }
        }
        raw.keys = keys.iter().map(|k| k.code()).collect();
        Ok(raw)
    }

    /// The battery percentage from `power_supply/ps-controller-battery-*`.
    pub fn battery_pct(&self) -> Option<u8> {
        let ps = self.hid_dir.as_ref()?.join("power_supply");
        std::fs::read_dir(ps)
            .ok()?
            .filter_map(Result::ok)
            .find_map(|e| read_u32(&e.path().join("capacity")))
            .map(|c| c.min(100) as u8)
    }

    pub fn controller(&self) -> Controller {
        Controller {
            id: self.id.clone(),
            name: self.name.clone(),
            connection: if self.bluetooth { "bluetooth" } else { "usb" }.to_string(),
            driver: self.driver.as_str().to_string(),
            battery_pct: self.battery_pct(),
        }
    }

    /// Set the lightbar (written only on change).
    pub fn set_lightbar(&mut self, rgb: [u8; 3]) {
        if self.last_rgb == Some(rgb) {
            return;
        }
        let Some(led) = &self.led else { return };
        let ok = std::fs::write(
            led.join("multi_intensity"),
            format!("{} {} {}", rgb[0], rgb[1], rgb[2]),
        )
        .and_then(|_| std::fs::write(led.join("brightness"), self.led_max.to_string()));
        match ok {
            Ok(()) => self.last_rgb = Some(rgb),
            Err(e) => tracing::debug!(error = %e, "lightbar write failed"),
        }
    }

    /// Play one rumble pulse.
    pub fn rumble(&mut self, p: Pulse) {
        if !self.rumble {
            return;
        }
        let mag = |m: f32| (m.clamp(0.0, 1.0) * f32::from(u16::MAX)) as u16;
        let data = FFEffectData {
            direction: 0,
            trigger: FFTrigger::default(),
            replay: FFReplay {
                length: p.length_ms,
                delay: 0,
            },
            kind: FFEffectKind::Rumble {
                strong_magnitude: mag(p.strong),
                weak_magnitude: mag(p.weak),
            },
        };
        self.playing = None;
        match self.dev.upload_ff_effect(data) {
            Ok(mut eff) => {
                if let Err(e) = eff.play(1) {
                    tracing::debug!(error = %e, "rumble play failed");
                }
                self.playing = Some(eff);
            }
            Err(e) => tracing::debug!(error = %e, "rumble upload failed"),
        }
    }
}

impl Drop for OpenPad {
    fn drop(&mut self) {
        // Leave the lightbar dark rather than showing a stale state.
        if let Some(led) = &self.led {
            let _ = std::fs::write(led.join("brightness"), "0");
        }
    }
}
