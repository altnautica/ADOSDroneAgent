//! The gauge binding sources: the agent config's `battery.gauge` first, then
//! the board dict.

use serde_json::json;

use super::*;
use crate::battery::ina226::tests::FakeRegs;

fn fake_ina226(_: &GaugeBinding) -> std::io::Result<Box<dyn I2cRegisters>> {
    Ok(Box::new(FakeRegs::ina226()))
}

fn feed(dir: &Path, config: Option<&str>, board: Option<serde_json::Value>) -> Feed {
    if let Some(yaml) = config {
        std::fs::write(dir.join("config.yaml"), yaml).unwrap();
    }
    if let Some(board) = board {
        std::fs::write(dir.join("board.json"), board.to_string()).unwrap();
    }
    Feed::new(
        StateIpcClient::disconnected(),
        dir.join("config.yaml"),
        dir.join("board.json"),
        fake_ina226,
    )
}

fn packs(feed: &Feed) -> usize {
    let engine = feed.engine.lock();
    engine.snapshot(epoch_ms(SystemTime::now())).packs.len()
}

const RIG_GAUGE: &str = "battery:\n  gauge:\n    type: ina226\n    i2c_bus: 8\n    addr: 64\n    \
                         shunt_ohm: 0.0005\n    max_current_a: 80\n";

#[test]
fn a_config_gauge_binds_without_a_board_dict() {
    let dir = tempfile::tempdir().unwrap();
    let mut feed = feed(dir.path(), Some(RIG_GAUGE), None);
    feed.step();
    assert!(matches!(feed.gauge, Gauge::Bound { .. }));
    assert_eq!(packs(&feed), 1);
    assert_eq!(
        feed.config_gauge,
        Ok(Some(GaugeBinding {
            i2c_bus: 8,
            addr: 64,
            shunt_ohm: 0.0005,
            max_current_a: 80.0,
        }))
    );
}

#[test]
fn a_config_gauge_without_its_shunt_values_is_absent_not_a_pack() {
    for missing in ["shunt_ohm", "max_current_a"] {
        let dir = tempfile::tempdir().unwrap();
        let yaml: String = RIG_GAUGE
            .lines()
            .filter(|l| !l.trim_start().starts_with(missing))
            .map(|l| format!("{l}\n"))
            .collect();
        // The board dict's gauge must not stand in for a broken rig binding.
        let board = json!({"battery_gauge": {"type": "ina226", "i2c_bus": 1, "addr": 64,
                                             "shunt_ohm": 0.1, "max_current_a": 0.8}});
        let mut feed = feed(dir.path(), Some(&yaml), Some(board));
        feed.step();
        let reason = feed.config_gauge.clone().unwrap_err();
        assert!(reason.contains(missing), "{reason}");
        assert!(matches!(feed.gauge, Gauge::Absent), "{missing}");
        assert_eq!(packs(&feed), 0, "{missing}");
    }
}

#[test]
fn the_config_gauge_wins_over_the_board_dict() {
    /// Only the rig binding's bus carries a device.
    fn bus8_only(binding: &GaugeBinding) -> std::io::Result<Box<dyn I2cRegisters>> {
        if binding.i2c_bus == 8 {
            fake_ina226(binding)
        } else {
            Err(std::io::Error::from(std::io::ErrorKind::NotFound))
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let board = json!({"battery_gauge": {"type": "ina226", "i2c_bus": 1, "addr": 65,
                                         "shunt_ohm": 0.1, "max_current_a": 0.8}});
    let mut feed = feed(dir.path(), Some(RIG_GAUGE), Some(board));
    feed.open_bus = bus8_only;
    feed.step();
    assert!(matches!(feed.gauge, Gauge::Bound { .. }));
    assert_eq!(packs(&feed), 1);
}

#[test]
fn adding_a_config_gauge_rebinds_on_the_next_tick() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    std::fs::write(&path, "battery:\n  low_cell_mv: 3600\n").unwrap();
    let t0 = SystemTime::now() - Duration::from_secs(60);
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(t0)
        .unwrap();
    let mut feed = feed(dir.path(), None, Some(json!({"name": "board"})));
    feed.step();
    assert!(matches!(feed.gauge, Gauge::Absent));

    std::fs::write(&path, RIG_GAUGE).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(t0 + Duration::from_secs(1))
        .unwrap();
    feed.step();
    assert!(matches!(feed.gauge, Gauge::Bound { .. }));
    assert_eq!(packs(&feed), 1);
}
