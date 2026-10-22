//! The `lora:` config block.
//!
//! Mirrors the Pydantic model `src/ados/core/config/lora.py`; the effective
//! config (schema defaults overlaid with `config.yaml`) is read through
//! `ados_config::schema::load_slice` by the daemon.

use serde::{Deserialize, Serialize};

use ados_lora::airtime::Modem;

/// `lora.e22`: the Ebyte UART module.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct E22Config {
    /// UART device the module is on.
    pub port: String,
    /// UART baud rate the module is configured for (REG0, default 9600).
    pub baud: u32,
    /// GPIO chip carrying the M0/M1 mode lines.
    pub gpio_chip: String,
    /// M0 and M1 mode line offsets; `None` when the lines are strapped.
    pub m0_gpio: Option<u32>,
    pub m1_gpio: Option<u32>,
    /// The fitted module's class, `850-930` (E22-900) or `410-493`
    /// (E22-400). Ebyte publishes no PID layout to read it from, so it is
    /// configured; the fitted reference part is the 900 class.
    pub module_band: String,
}

impl Default for E22Config {
    fn default() -> Self {
        Self {
            port: "/dev/ttyUSB0".into(),
            baud: 9600,
            gpio_chip: "gpiochip0".into(),
            m0_gpio: None,
            m1_gpio: None,
            module_band: "850-930".into(),
        }
    }
}

impl E22Config {
    /// The M0/M1 mode lines are not both wired, so the module cannot enter
    /// configuration mode.
    pub fn strapped(&self) -> bool {
        self.m0_gpio.is_none() || self.m1_gpio.is_none()
    }
}

/// `lora.meshtastic`: the Meshtastic node bridge.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MeshtasticConfig {
    pub enabled: bool,
    /// Serial device of the node (its USB CDC port).
    pub port: Option<String>,
}

/// `lora.loopback`: the UDP stand-in radio for SITL.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LoopbackConfig {
    /// Local UDP address the lane listens on.
    pub bind: String,
    /// UDP address frames are sent to (the peer script).
    pub peer: String,
    /// The `module_band` the loopback radio reports (`any`, `850-930`,
    /// `410-493`, `863-928`), so the band check can be exercised.
    pub module_band: String,
}

impl Default for LoopbackConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:7810".into(),
            peer: "127.0.0.1:7811".into(),
            module_band: "any".into(),
        }
    }
}

/// The whole `lora:` block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LoraConfig {
    pub enabled: bool,
    /// `e22 | meshtastic | loopback`.
    pub transport: String,
    /// Region code (`IN`, `EU`, `EU433`, `AU433`, or any ISO 3166-1 alpha-2
    /// country); `None` follows `network.regulatory.region`.
    pub region: Option<String>,
    /// This node's id on the lane.
    pub node_id: u16,
    /// Fixed channel centre; `None` uses the plan default (or hops where the
    /// plan hops).
    pub freq_mhz: Option<f64>,
    /// Requested ERP; `None` runs at the plan clamp.
    pub erp_mw: Option<f64>,
    pub sf: u8,
    pub bw_khz: u32,
    /// Coding rate index 1..=4 (4/5..4/8).
    pub cr: u8,
    pub preamble: u16,
    /// E22 air data rate; only `2.4k` is in plan until the per-rate modem
    /// parameters are measured.
    pub air_rate: String,
    pub antenna_gain_dbi: f64,
    pub cable_loss_db: f64,
    pub beacon_interval_s: u32,
    /// The persisted airtime ledger.
    pub ledger_path: String,
    pub e22: E22Config,
    pub meshtastic: MeshtasticConfig,
    pub loopback: LoopbackConfig,
}

impl Default for LoraConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            transport: "e22".into(),
            region: None,
            node_id: 1,
            freq_mhz: None,
            erp_mw: None,
            sf: 9,
            bw_khz: 125,
            cr: 1,
            preamble: 8,
            air_rate: "2.4k".into(),
            antenna_gain_dbi: 2.15,
            cable_loss_db: 0.0,
            beacon_interval_s: 30,
            ledger_path: "/var/ados/lora/airtime.jsonl".into(),
            e22: E22Config::default(),
            meshtastic: MeshtasticConfig::default(),
            loopback: LoopbackConfig::default(),
        }
    }
}

impl LoraConfig {
    /// The explicit modem parameters (loopback, and the E22's assumed set).
    pub fn modem(&self) -> Modem {
        Modem {
            sf: self.sf,
            bw_khz: self.bw_khz,
            cr: self.cr,
            preamble: self.preamble,
            crc: true,
            implicit_header: false,
        }
    }
}

#[cfg(all(test, feature = "daemon"))]
mod tests {
    use super::*;

    #[test]
    fn config_slice_conforms() {
        ados_config::schema::assert_slice_conforms::<LoraConfig>("/lora");
    }
}
