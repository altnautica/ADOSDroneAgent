//! OLED frame upload with brownout recovery and change-only contrast writes.
//!
//! A panel brownout resets the controller to display-off; without the init
//! sequence every later upload lands on a dark panel. [`OledLink`] counts
//! consecutive upload failures and re-runs [`init`] on the same bus after
//! [`REINIT_AFTER_FAILURES`]. It also applies the configured contrast only when
//! the value changes (and again after a re-init, which restores the power-on
//! default).

use crate::oled::{init, render_frame, write_cmd, Controller, I2cBus, OledGeometry};

/// The SSD1306/SH1106 "set contrast" command and its one-byte argument.
pub fn contrast_command(value: u8) -> [u8; 2] {
    [0x81, value]
}

/// Map the persisted OLED `brightness` (the native 0-255 contrast scale the
/// config and API validate) onto the controller's contrast byte, clamping an
/// out-of-range stored value.
pub fn brightness_to_contrast(brightness: i64) -> u8 {
    brightness.clamp(0, 255) as u8
}

/// Consecutive frame-upload failures after which the panel is re-initialised.
pub const REINIT_AFTER_FAILURES: u32 = 5;

/// Frame upload with brownout recovery and change-only contrast writes.
#[derive(Debug, Default)]
pub struct OledLink {
    failures: u32,
    applied_contrast: Option<u8>,
}

impl OledLink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Upload `packed`. After [`REINIT_AFTER_FAILURES`] consecutive failures,
    /// re-run [`init`] on the same bus and re-apply the contrast on the next
    /// [`OledLink::apply_contrast`].
    pub fn upload<B: I2cBus>(
        &mut self,
        bus: &mut B,
        packed: &[u8],
        geom: OledGeometry,
        controller: Controller,
    ) -> std::io::Result<()> {
        match render_frame(bus, packed, geom, controller) {
            Ok(()) => {
                self.failures = 0;
                Ok(())
            }
            Err(e) => {
                self.failures += 1;
                if self.failures >= REINIT_AFTER_FAILURES {
                    self.failures = 0;
                    match init(bus, geom) {
                        Ok(()) => {
                            tracing::info!("OLED re-initialised after repeated upload failures");
                            self.applied_contrast = None;
                        }
                        Err(init_err) => {
                            tracing::warn!(error = %init_err, "OLED re-init failed; will retry");
                        }
                    }
                }
                Err(e)
            }
        }
    }

    /// Send the contrast command when `value` differs from what the panel holds.
    /// Returns whether a command was sent.
    pub fn apply_contrast<B: I2cBus>(&mut self, bus: &mut B, value: u8) -> std::io::Result<bool> {
        if self.applied_contrast == Some(value) {
            return Ok(false);
        }
        write_cmd(bus, &contrast_command(value))?;
        self.applied_contrast = Some(value);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oled::{init_commands, CTRL_CMD, CTRL_DATA};

    /// Records writes; data writes fail while `failing` is set.
    #[derive(Default)]
    struct FlakyBus {
        failing: bool,
        frames: Vec<Vec<u8>>,
    }

    impl I2cBus for FlakyBus {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
            if self.failing && bytes[0] == CTRL_DATA {
                return Err(std::io::Error::other("EREMOTEIO"));
            }
            self.frames.push(bytes.to_vec());
            Ok(())
        }
    }

    fn init_count(bus: &FlakyBus) -> usize {
        let mut init = vec![CTRL_CMD];
        init.extend(init_commands(OledGeometry::W128_H32));
        bus.frames.iter().filter(|f| **f == init).count()
    }

    #[test]
    fn oled_reinit_after_five_failures() {
        let geom = OledGeometry::W128_H32;
        let packed = vec![0u8; geom.frame_bytes()];
        let mut bus = FlakyBus {
            failing: true,
            ..Default::default()
        };
        let mut link = OledLink::new();
        link.apply_contrast(&mut bus, 100).unwrap();

        for _ in 0..REINIT_AFTER_FAILURES - 1 {
            assert!(link
                .upload(&mut bus, &packed, geom, Controller::Ssd1306)
                .is_err());
        }
        assert_eq!(init_count(&bus), 0, "no re-init before the fifth failure");
        assert!(link
            .upload(&mut bus, &packed, geom, Controller::Ssd1306)
            .is_err());
        assert_eq!(init_count(&bus), 1, "the fifth failure re-inits the panel");

        // The re-init reset the contrast, so the same value is sent again.
        assert!(link.apply_contrast(&mut bus, 100).unwrap());
        bus.failing = false;
        assert!(link
            .upload(&mut bus, &packed, geom, Controller::Ssd1306)
            .is_ok());
    }

    #[test]
    fn brightness_maps_to_contrast_command() {
        assert_eq!(contrast_command(brightness_to_contrast(204)), [0x81, 204]);
        assert_eq!(brightness_to_contrast(0), 0);
        assert_eq!(brightness_to_contrast(255), 255);
        assert_eq!(brightness_to_contrast(999), 255);
        assert_eq!(brightness_to_contrast(-5), 0);

        let mut bus = FlakyBus::default();
        let mut link = OledLink::new();
        assert!(link.apply_contrast(&mut bus, 128).unwrap());
        assert!(
            !link.apply_contrast(&mut bus, 128).unwrap(),
            "unchanged value is not resent"
        );
        assert!(link.apply_contrast(&mut bus, 40).unwrap());
        assert_eq!(
            bus.frames,
            vec![vec![CTRL_CMD, 0x81, 128], vec![CTRL_CMD, 0x81, 40]]
        );
    }
}
