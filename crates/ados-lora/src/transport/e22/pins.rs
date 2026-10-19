//! The E22 M0/M1 mode lines (manual v1.3 section 6: M1 M0 = `00` normal,
//! `01` WOR, `10` configuration, `11` deep sleep).

use std::io;

/// The two operating modes the transport uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Mode 0: UART and radio open (M0 = 0, M1 = 0).
    Normal,
    /// Mode 2: register access over the UART at 9600 8N1, radio off
    /// (M0 = 0, M1 = 1).
    Config,
}

impl Mode {
    /// `(M0, M1)` line levels, high = `true`.
    pub fn levels(self) -> (bool, bool) {
        match self {
            Mode::Normal => (false, false),
            Mode::Config => (false, true),
        }
    }
}

/// Something that drives M0 and M1.
pub trait ModePins {
    fn set_mode(&mut self, mode: Mode) -> io::Result<()>;
}

/// The chip index in a `gpiochipN` / `/dev/gpiochipN` name.
pub fn chip_index(name: &str) -> io::Result<u32> {
    name.trim_start_matches("/dev/")
        .strip_prefix("gpiochip")
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("lora.e22.gpio_chip {name:?} is not a gpiochipN name"),
            )
        })
}

/// M0/M1 on host GPIO lines through the Linux GPIO character device, with the
/// same driver the GPIO output service uses. Lines are requested low, so the
/// module starts in normal mode.
#[cfg(target_os = "linux")]
pub struct GpioModePins {
    out: ados_gpio::GpioOutput,
    chip: u32,
    m0: u32,
    m1: u32,
}

#[cfg(target_os = "linux")]
impl GpioModePins {
    pub fn open(chip: &str, m0: u32, m1: u32) -> io::Result<Self> {
        let mut pins = Self {
            out: ados_gpio::GpioOutput::new(),
            chip: chip_index(chip)?,
            m0,
            m1,
        };
        pins.set_mode(Mode::Normal)?;
        Ok(pins)
    }
}

#[cfg(target_os = "linux")]
impl ModePins for GpioModePins {
    fn set_mode(&mut self, mode: Mode) -> io::Result<()> {
        let level = |high: bool| {
            if high {
                ados_gpio::Level::High
            } else {
                ados_gpio::Level::Low
            }
        };
        let (m0, m1) = mode.levels();
        for (pin, high) in [(self.m0, m0), (self.m1, m1)] {
            self.out
                .set(self.chip, pin, level(high))
                .map_err(|e| io::Error::other(format!("E22 mode line {pin}: {e}")))?;
        }
        Ok(())
    }
}

/// Off Linux there is no GPIO character device: opening mode lines fails, so
/// only a module with strapped M0/M1 can be used.
#[cfg(not(target_os = "linux"))]
pub enum GpioModePins {}

#[cfg(not(target_os = "linux"))]
impl GpioModePins {
    pub fn open(_chip: &str, _m0: u32, _m1: u32) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "E22 mode lines need the Linux GPIO character device",
        ))
    }
}

#[cfg(not(target_os = "linux"))]
impl ModePins for GpioModePins {
    fn set_mode(&mut self, _mode: Mode) -> io::Result<()> {
        match *self {}
    }
}
