//! Linux GPIO-output line driver over `gpio-cdev`.
//!
//! Opens a `/dev/gpiochip*` line in OUTPUT mode and drives it high/low. This is
//! the only hardware-coupled part of the crate; it is compiled on Linux only so
//! the timing model and the command parser in the parent module still build and
//! test on a non-Linux dev host. The same pure-Rust character-device backend the
//! front-panel button reader uses (no libgpiod C dependency).
//!
//! Each [`GpioOutput`] holds one requested line. The line is requested with an
//! initial LOW value, so claiming a pin never briefly energizes a buzzer/LED
//! before the first command — safe-by-default at the hardware boundary too.

use std::collections::BTreeMap;

use gpio_cdev::{Chip, LineHandle, LineRequestFlags};

use crate::Level;

/// A consumer label the kernel records on the requested line, so a peek at the
/// GPIO subsystem shows who owns it.
const CONSUMER: &str = "ados-gpio";

/// A driven output line plus its last-written level, keyed by `(chip, pin)`.
struct DrivenLine {
    handle: LineHandle,
    level: Level,
}

/// Owns the set of output lines the service is driving, opening each chip lazily
/// on first use and caching the line handle so repeated writes reuse it.
///
/// Safe-by-default: nothing is driven until [`set`](Self::set) (or
/// [`pulse`](Self::pulse)) is called, and a line is requested with an initial
/// LOW value so claiming it never energizes the attached device.
#[derive(Default)]
pub struct GpioOutput {
    /// Open chips, keyed by chip index (the `N` in `/dev/gpiochipN`).
    chips: BTreeMap<u32, Chip>,
    /// Requested output lines, keyed by `(chip, pin)`.
    lines: BTreeMap<(u32, u32), DrivenLine>,
}

impl GpioOutput {
    /// A driver with no chips opened and no lines driven.
    pub fn new() -> Self {
        Self::default()
    }

    /// Drive `(chip, pin)` to `level`, requesting the line on first use. The
    /// line is requested with an initial LOW value, so the first `set High`
    /// transitions cleanly from low. Reuses a cached handle on repeat writes.
    pub fn set(&mut self, chip: u32, pin: u32, level: Level) -> anyhow::Result<()> {
        let key = (chip, pin);
        if !self.lines.contains_key(&key) {
            let handle = self.request_line(chip, pin)?;
            self.lines.insert(
                key,
                DrivenLine {
                    handle,
                    level: Level::Low,
                },
            );
        }
        let line = self.lines.get_mut(&key).expect("just inserted");
        line.handle.set_value(level.value())?;
        line.level = level;
        Ok(())
    }

    /// Request an output line on `chip`, opening the chip if needed. The line
    /// starts LOW so the request never briefly drives a buzzer/LED.
    fn request_line(&mut self, chip: u32, pin: u32) -> anyhow::Result<LineHandle> {
        let dev = format!("/dev/gpiochip{chip}");
        let chip_ref = match self.chips.get_mut(&chip) {
            Some(c) => c,
            None => {
                let opened = Chip::new(&dev).map_err(|e| anyhow::anyhow!("open {dev}: {e}"))?;
                self.chips.entry(chip).or_insert(opened)
            }
        };
        let line = chip_ref
            .get_line(pin)
            .map_err(|e| anyhow::anyhow!("get line {pin} on {dev}: {e}"))?;
        let handle = line
            .request(LineRequestFlags::OUTPUT, Level::Low.value(), CONSUMER)
            .map_err(|e| anyhow::anyhow!("request output {pin} on {dev}: {e}"))?;
        Ok(handle)
    }

    /// The level a driven line is currently held at, or `None` when the line has
    /// never been driven (it is not owned by this service).
    pub fn level_of(&self, chip: u32, pin: u32) -> Option<Level> {
        self.lines.get(&(chip, pin)).map(|l| l.level)
    }

    /// A snapshot of every driven line and its level, for the status sidecar.
    pub fn snapshot(&self) -> Vec<(u32, u32, Level)> {
        self.lines
            .iter()
            .map(|((chip, pin), l)| (*chip, *pin, l.level))
            .collect()
    }

    /// Drive every owned line LOW. Called on shutdown so the service never
    /// leaves a buzzer/LED energized when it stops.
    pub fn all_low(&mut self) {
        for ((chip, pin), line) in self.lines.iter_mut() {
            if let Err(e) = line.handle.set_value(Level::Low.value()) {
                tracing::debug!(chip, pin, error = %e, "failed to drive line low on shutdown");
            } else {
                line.level = Level::Low;
            }
        }
    }
}
