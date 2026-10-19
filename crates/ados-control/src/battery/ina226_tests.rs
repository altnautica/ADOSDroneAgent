use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::json;

use super::*;

/// A register file standing in for the device. Writes land in the map and are
/// also recorded, shared through `log` so a test can inspect them after the
/// bus moved into the driver.
#[derive(Default)]
pub(crate) struct FakeRegs {
    pub regs: BTreeMap<u8, u16>,
    pub log: Arc<Mutex<Vec<(u8, u16)>>>,
}

impl FakeRegs {
    /// A genuine INA226 reading 16.8 V, +10 mV shunt, 20 A, 5505 power counts
    /// (the 0.5 mΩ / 80 A binding).
    pub(crate) fn ina226() -> Self {
        let mut regs = BTreeMap::new();
        regs.insert(REG_MANUFACTURER_ID, MANUFACTURER_ID);
        regs.insert(REG_DIE_ID, DIE_ID);
        regs.insert(REG_BUS_VOLTAGE, 13440);
        regs.insert(REG_SHUNT_VOLTAGE, 4000);
        regs.insert(REG_CURRENT, 0x2000);
        regs.insert(REG_POWER, 5505);
        Self {
            regs,
            log: Arc::default(),
        }
    }
}

impl I2cRegisters for FakeRegs {
    fn read_reg(&mut self, reg: u8) -> std::io::Result<u16> {
        self.regs
            .get(&reg)
            .copied()
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::TimedOut))
    }

    fn write_reg(&mut self, reg: u8, value: u16) -> std::io::Result<()> {
        self.regs.insert(reg, value);
        self.log.lock().unwrap().push((reg, value));
        Ok(())
    }
}

pub(crate) fn binding() -> GaugeBinding {
    GaugeBinding {
        i2c_bus: 1,
        addr: 0x40,
        shunt_ohm: 0.0005,
        max_current_a: 80.0,
    }
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

#[test]
fn shunt_voltage_decodes_signed() {
    assert!(close(decode_shunt_v(4000), 0.010));
    assert!(close(decode_shunt_v((-4000_i16) as u16), -0.010));
    assert!(close(decode_shunt_v(0x7FFF), 32767.0 * 2.5e-6));
    assert!(close(decode_shunt_v(0x8000), -32768.0 * 2.5e-6));
}

#[test]
fn bus_voltage_decodes_at_1_25_mv() {
    assert!(close(decode_bus_v(13440), 16.8));
    assert!(close(decode_bus_v(0), 0.0));
    assert!(close(decode_bus_v(1), 0.00125));
}

#[test]
fn calibration_matches_the_datasheet_formula() {
    // 1 mA per count on a 2 mΩ shunt: 0.00512 / (0.001 × 0.002) = 2560.
    assert_eq!(calibration(0.001, 0.002), Some(2560));
    // The 0.5 mΩ / 80 A binding: 0.00512 / (80/32768 × 0.0005) = 4194.3.
    assert_eq!(calibration(binding().current_lsb(), 0.0005), Some(4194));
    // Too fine a resolution overflows the 15-bit register.
    assert_eq!(calibration(1e-6, 0.0005), None);
    // Too coarse rounds to zero.
    assert_eq!(calibration(100.0, 1.0), None);
}

#[test]
fn current_and_power_decode_with_the_computed_lsb() {
    let lsb = binding().current_lsb();
    assert!(close(decode_current_a(0x2000, lsb), 20.0));
    assert!(close(decode_current_a((-8192_i16) as u16, lsb), -20.0));
    assert!(close(decode_power_w(5505, lsb), 5505.0 * 25.0 * lsb));
    assert!((decode_power_w(5505, lsb) - 336.0).abs() < 0.07);
}

#[test]
fn bring_up_writes_config_and_calibration_then_reads() {
    let fake = FakeRegs::ina226();
    let log = fake.log.clone();
    let mut gauge = Ina226::bring_up(Box::new(fake), &binding()).unwrap();
    assert_eq!(
        *log.lock().unwrap(),
        vec![(REG_CONFIG, CONFIG_VALUE), (REG_CALIBRATION, 4194)]
    );
    let r = gauge.read().unwrap();
    assert!(close(r.bus_v, 16.8));
    assert!(close(r.shunt_v, 0.010));
    assert!(close(r.current_a, 20.0));
    let sample = r.to_sample(1_000);
    assert_eq!(sample.source, PackSource::Ina226);
    assert_eq!(sample.voltage_v, Some(16.8));
    assert_eq!(sample.current_a, Some(20.0));
    assert_eq!(sample.power_w, Some(335.999));
    assert!(sample.cells.is_empty() && !sample.cells_plausible);
    assert_eq!(sample.remaining_pct, None);
}

#[test]
fn a_wrong_manufacturer_id_is_rejected_without_writes() {
    let mut fake = FakeRegs::ina226();
    fake.regs.insert(REG_MANUFACTURER_ID, 0x1234);
    let log = fake.log.clone();
    let err = Ina226::bring_up(Box::new(fake), &binding())
        .err()
        .expect("rejected");
    assert!(err.contains("manufacturer id"), "{err}");
    assert!(log.lock().unwrap().is_empty());
}

#[test]
fn a_wrong_die_id_is_rejected() {
    let mut fake = FakeRegs::ina226();
    fake.regs.insert(REG_DIE_ID, 0x2270);
    let err = Ina226::bring_up(Box::new(fake), &binding())
        .err()
        .expect("rejected");
    assert!(err.contains("die id"), "{err}");
}

#[test]
fn binding_parses_from_the_board_dict() {
    let board = json!({"battery_gauge": {"type": "ina226", "i2c_bus": 1, "addr": 64,
                                         "shunt_ohm": 0.0005, "max_current_a": 80}});
    assert_eq!(GaugeBinding::from_board(&board), Ok(Some(binding())));
    assert_eq!(GaugeBinding::from_board(&json!({})), Ok(None));
    assert_eq!(
        GaugeBinding::from_board(&json!({"battery_gauge": null})),
        Ok(None)
    );
    assert!(GaugeBinding::from_board(&json!({"battery_gauge": {"type": "ina219"}})).is_err());
    let no_shunt = json!({"battery_gauge": {"type": "ina226", "i2c_bus": 1, "addr": 64,
                                            "max_current_a": 80}});
    assert!(GaugeBinding::from_board(&no_shunt).is_err());
    let bad_addr = json!({"battery_gauge": {"type": "ina226", "i2c_bus": 1, "addr": 300,
                                            "shunt_ohm": 0.0005, "max_current_a": 80}});
    assert!(GaugeBinding::from_board(&bad_addr).is_err());
}
