//! INA226 shunt/bus power monitor over Linux I2C: the rig's battery gauge.
//!
//! The binding is per rig, because the shunt resistor and its full-scale
//! current belong to the rig's power wiring: the agent config's
//! `battery.gauge` block, else the HAL board dict (`/run/ados/board.json`,
//! key `battery_gauge`). Both carry the same shape, every key required:
//!
//! ```json
//! {"type":"ina226","i2c_bus":1,"addr":64,"shunt_ohm":0.0005,"max_current_a":80}
//! ```
//!
//! Registers are 16-bit, big-endian on the wire. The current LSB is
//! `max_current_a / 2^15` and the calibration register is
//! `CAL = 0.00512 / (current_lsb × shunt_ohm)`; current then reads in
//! `current_lsb` units and power in `25 × current_lsb` units. The register
//! access sits behind [`I2cRegisters`] so the decode and the bring-up checks
//! run against a fake bus in tests.

use serde_json::Value;

use super::{PackSource, Sample};

pub const REG_CONFIG: u8 = 0x00;
pub const REG_SHUNT_VOLTAGE: u8 = 0x01;
pub const REG_BUS_VOLTAGE: u8 = 0x02;
pub const REG_POWER: u8 = 0x03;
pub const REG_CURRENT: u8 = 0x04;
pub const REG_CALIBRATION: u8 = 0x05;
pub const REG_MANUFACTURER_ID: u8 = 0xFE;
pub const REG_DIE_ID: u8 = 0xFF;

/// The manufacturer id register value every INA226 reads.
pub const MANUFACTURER_ID: u16 = 0x5449;
/// The die id register value (device id in bits 15..4, revision in 3..0).
pub const DIE_ID: u16 = 0x2260;

/// Shunt voltage LSB: 2.5 µV.
pub const SHUNT_LSB_V: f64 = 2.5e-6;
/// Bus voltage LSB: 1.25 mV.
pub const BUS_LSB_V: f64 = 1.25e-3;
/// Power LSB as a multiple of the current LSB.
pub const POWER_LSB_FACTOR: f64 = 25.0;
/// The calibration constant of the datasheet formula.
const CAL_CONSTANT: f64 = 0.00512;

/// Configuration written at bring-up: 16-sample averaging, 1.1 ms bus and
/// shunt conversion times, continuous shunt + bus measurement.
pub const CONFIG_VALUE: u16 = 0x4527;

/// 16-bit register access to one device on an I2C bus.
pub trait I2cRegisters: Send {
    fn read_reg(&mut self, reg: u8) -> std::io::Result<u16>;
    fn write_reg(&mut self, reg: u8, value: u16) -> std::io::Result<()>;
}

/// A `battery.gauge` / `battery_gauge` binding.
#[derive(Debug, Clone, PartialEq)]
pub struct GaugeBinding {
    pub i2c_bus: u8,
    pub addr: u16,
    pub shunt_ohm: f64,
    pub max_current_a: f64,
}

impl GaugeBinding {
    /// The INA226 binding of a board dict: `Ok(None)` when the board declares
    /// no gauge, `Err` when it declares one this reader cannot use.
    pub fn from_board(board: &Value) -> Result<Option<Self>, String> {
        Self::from_block(board.get("battery_gauge"))
    }

    /// The `battery.gauge` binding of the agent config at `path`: `Ok(None)`
    /// when the file or the block is absent, `Err` when the file does not parse
    /// or the block is one this reader cannot use (a missing `shunt_ohm` or
    /// `max_current_a` included).
    pub fn from_config_file(path: &std::path::Path) -> Result<Option<Self>, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("config unreadable: {e}")),
        };
        let root: Value =
            serde_norway::from_str(&text).map_err(|e| format!("config unparseable: {e}"))?;
        Self::from_block(root.get("battery").and_then(|b| b.get("gauge")))
    }

    fn from_block(gauge: Option<&Value>) -> Result<Option<Self>, String> {
        let Some(gauge) = gauge.filter(|g| !g.is_null()) else {
            return Ok(None);
        };
        let kind = gauge.get("type").and_then(Value::as_str).unwrap_or("");
        if kind != "ina226" {
            return Err(format!("unsupported gauge type {kind:?}"));
        }
        let int = |key: &str| gauge.get(key).and_then(Value::as_u64);
        let float = |key: &str| {
            gauge
                .get(key)
                .and_then(Value::as_f64)
                .filter(|v| v.is_finite() && *v > 0.0)
        };
        let i2c_bus = int("i2c_bus")
            .and_then(|b| u8::try_from(b).ok())
            .ok_or("missing or invalid i2c_bus")?;
        let addr = int("addr")
            .filter(|a| (0x03..=0x77).contains(a))
            .ok_or("missing or invalid addr")? as u16;
        let shunt_ohm = float("shunt_ohm").ok_or("missing or invalid shunt_ohm")?;
        let max_current_a = float("max_current_a").ok_or("missing or invalid max_current_a")?;
        Ok(Some(Self {
            i2c_bus,
            addr,
            shunt_ohm,
            max_current_a,
        }))
    }

    /// Amps per current-register count.
    pub fn current_lsb(&self) -> f64 {
        self.max_current_a / 32768.0
    }
}

/// The calibration register value for a current LSB and shunt, `None` when
/// it does not fit the register's 15 bits or rounds to zero.
pub fn calibration(current_lsb: f64, shunt_ohm: f64) -> Option<u16> {
    let cal = (CAL_CONSTANT / (current_lsb * shunt_ohm)).trunc();
    (cal >= 1.0 && cal <= f64::from(0x7FFF_u16)).then_some(cal as u16)
}

/// Shunt voltage in volts (signed register).
pub fn decode_shunt_v(raw: u16) -> f64 {
    f64::from(raw as i16) * SHUNT_LSB_V
}

/// Bus voltage in volts (unsigned register).
pub fn decode_bus_v(raw: u16) -> f64 {
    f64::from(raw) * BUS_LSB_V
}

/// Current in amps (signed register).
pub fn decode_current_a(raw: u16, current_lsb: f64) -> f64 {
    f64::from(raw as i16) * current_lsb
}

/// Power in watts (unsigned register).
pub fn decode_power_w(raw: u16, current_lsb: f64) -> f64 {
    f64::from(raw) * POWER_LSB_FACTOR * current_lsb
}

/// One measurement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GaugeReading {
    pub bus_v: f64,
    pub shunt_v: f64,
    pub current_a: f64,
    pub power_w: f64,
}

impl GaugeReading {
    /// The engine sample for this reading at `t_ms`. The gauge measures the
    /// pack as a whole: no cells, no state of charge.
    pub fn to_sample(self, t_ms: i64) -> Sample {
        Sample {
            t_ms,
            cells: Vec::new(),
            cells_plausible: false,
            voltage_v: Some(round_to(self.bus_v, 1000.0)),
            current_a: Some(round_to(self.current_a, 1000.0)),
            power_w: Some(round_to(self.power_w, 1000.0)),
            remaining_pct: None,
            temperature_c: None,
            consumed_mah: None,
            consumed_wh: None,
            source: PackSource::Ina226,
        }
    }
}

fn round_to(v: f64, scale: f64) -> f64 {
    (v * scale).round() / scale
}

/// A verified, calibrated INA226.
pub struct Ina226 {
    bus: Box<dyn I2cRegisters>,
    current_lsb: f64,
}

impl Ina226 {
    /// Verify the device identity, then write the configuration and the
    /// calibration for `binding`. Any mismatch is an error: a device that is
    /// not an INA226 never produces a pack.
    pub fn bring_up(
        mut bus: Box<dyn I2cRegisters>,
        binding: &GaugeBinding,
    ) -> Result<Self, String> {
        let io = |e: std::io::Error| format!("i2c: {e}");
        let manufacturer = bus.read_reg(REG_MANUFACTURER_ID).map_err(io)?;
        if manufacturer != MANUFACTURER_ID {
            return Err(format!(
                "manufacturer id {manufacturer:#06x}, expected {MANUFACTURER_ID:#06x}"
            ));
        }
        let die = bus.read_reg(REG_DIE_ID).map_err(io)?;
        if die >> 4 != DIE_ID >> 4 {
            return Err(format!("die id {die:#06x}, expected {DIE_ID:#06x}"));
        }
        let current_lsb = binding.current_lsb();
        let cal = calibration(current_lsb, binding.shunt_ohm)
            .ok_or("calibration out of range for this shunt and current")?;
        bus.write_reg(REG_CONFIG, CONFIG_VALUE).map_err(io)?;
        bus.write_reg(REG_CALIBRATION, cal).map_err(io)?;
        Ok(Self { bus, current_lsb })
    }

    pub fn read(&mut self) -> std::io::Result<GaugeReading> {
        Ok(GaugeReading {
            bus_v: decode_bus_v(self.bus.read_reg(REG_BUS_VOLTAGE)?),
            shunt_v: decode_shunt_v(self.bus.read_reg(REG_SHUNT_VOLTAGE)?),
            current_a: decode_current_a(self.bus.read_reg(REG_CURRENT)?, self.current_lsb),
            power_w: decode_power_w(self.bus.read_reg(REG_POWER)?, self.current_lsb),
        })
    }
}

/// Open the device of `binding` on the host's I2C bus.
#[cfg(target_os = "linux")]
pub fn open_bus(binding: &GaugeBinding) -> std::io::Result<Box<dyn I2cRegisters>> {
    Ok(Box::new(linux::I2cDev::open(
        binding.i2c_bus,
        binding.addr,
    )?))
}

/// I2C is a Linux-only edge; elsewhere the gauge is unavailable.
#[cfg(not(target_os = "linux"))]
pub fn open_bus(_binding: &GaugeBinding) -> std::io::Result<Box<dyn I2cRegisters>> {
    Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;

    /// `I2C_SLAVE` ioctl request: bind the slave address for the fd's reads
    /// and writes. The request arg type differs across libc flavours, so it
    /// is cast at the call site.
    const I2C_SLAVE: u32 = 0x0703;

    /// A `/dev/i2c-N` fd bound to one slave address.
    pub(super) struct I2cDev {
        file: std::fs::File,
    }

    impl I2cDev {
        pub(super) fn open(bus: u8, addr: u16) -> std::io::Result<Self> {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(format!("/dev/i2c-{bus}"))?;
            // SAFETY: I2C_SLAVE takes the address as the integer ioctl arg
            // (not a pointer); the fd is a freshly opened i2c device we own.
            let rc = unsafe {
                nix::libc::ioctl(file.as_raw_fd(), I2C_SLAVE as _, addr as nix::libc::c_int)
            };
            if rc < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(Self { file })
        }
    }

    impl super::I2cRegisters for I2cDev {
        fn read_reg(&mut self, reg: u8) -> std::io::Result<u16> {
            self.file.write_all(&[reg])?;
            let mut buf = [0u8; 2];
            self.file.read_exact(&mut buf)?;
            Ok(u16::from_be_bytes(buf))
        }

        fn write_reg(&mut self, reg: u8, value: u16) -> std::io::Result<()> {
            let [hi, lo] = value.to_be_bytes();
            self.file.write_all(&[reg, hi, lo])
        }
    }
}

#[cfg(test)]
#[path = "ina226_tests.rs"]
pub(crate) mod tests;
