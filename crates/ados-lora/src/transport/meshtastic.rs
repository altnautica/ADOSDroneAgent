//! A Meshtastic node on its serial client API.
//!
//! The node owns its radio: it sits on one preset slot of its region, runs its
//! own channel-activity detection and rebroadcasts other nodes' packets. The
//! bridge sets the node's region and transmit power to the plan
//! ([`Meshtastic::apply_plan`]), forwards the lane's beacons as position
//! packets and text frames as text messages, and hands positions heard from
//! other nodes up as [`RxEvent::MeshPosition`]. The node's own
//! `DeviceMetrics.air_util_tx` is reported so the governor can account
//! rebroadcasts it did not originate.

mod msg;
mod pb;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::io;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_serial::{SerialPortBuilderExt, SerialStream};

use crate::airtime::Modem;
use crate::band::ModuleCaps;
use crate::config::LoraConfig;
use crate::frame::{Beacon, Frame, Kind};
use crate::transport::{RxEvent, Transport, TransportKind, TxFrame};
use msg::{port, FromRadio, LoraSettings, OutPacket, Position};

/// The node's serial console rate.
const BAUD: u32 = 115_200;
/// Longest wait for the configuration download at start.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// The firmware's preamble length (`RadioInterface.h`, `preambleLength`).
const PREAMBLE: u16 = 16;
/// Hop limit used when the node reports none (the firmware default).
const DEFAULT_HOP_LIMIT: u32 = 3;
/// Nodes whose last battery level is remembered.
const MAX_BATTERY_NODES: usize = 256;

/// mesh.proto `HardwareModel` values of SX1262 boards covering 863..928 MHz.
const SX1262_863_928: &[(u32, &str)] = &[
    (7, "T_ECHO"),
    (9, "RAK4631"),
    (43, "HELTEC_V3"),
    (44, "HELTEC_WSL_V3"),
    (48, "HELTEC_WIRELESS_TRACKER"),
    (49, "HELTEC_WIRELESS_PAPER"),
    (58, "HELTEC_WIRELESS_TRACKER_V1_0"),
    (110, "HELTEC_V4"),
];

/// Plan preset names to config.proto `Config.LoRaConfig.RegionCode`.
const REGIONS: &[(&str, u32)] = &[
    ("US", 1),
    ("EU_433", 2),
    ("EU_868", 3),
    ("JP", 5),
    ("ANZ", 6),
    ("KR", 7),
    ("TW", 8),
    ("RU", 9),
    ("IN", 10),
    ("TH", 12),
    ("MY_919", 17),
    ("ANZ_433", 22),
    ("BR_902", 26),
];

/// The RegionCode for a plan's Meshtastic preset name.
pub fn region_code(name: &str) -> Option<u32> {
    REGIONS.iter().find(|(n, _)| *n == name).map(|(_, c)| *c)
}

/// SF, bandwidth (kHz) and coding-rate denominator of a config.proto
/// `ModemPreset`, from the firmware's `modemPresetToParams`
/// (`src/mesh/MeshRadio.h`, non-wide LoRa). Presets that function does not
/// list fall to its default, LONG_FAST.
fn preset_params(preset: u32) -> (u8, f64, u8) {
    match preset {
        8 => (7, 500.0, 5),  // SHORT_TURBO
        6 => (7, 250.0, 5),  // SHORT_FAST
        5 => (8, 250.0, 5),  // SHORT_SLOW
        4 => (9, 250.0, 5),  // MEDIUM_FAST
        3 => (10, 250.0, 5), // MEDIUM_SLOW
        9 => (11, 500.0, 8), // LONG_TURBO
        7 => (11, 125.0, 8), // LONG_MODERATE
        1 => (12, 125.0, 8), // LONG_SLOW
        _ => (11, 250.0, 5), // LONG_FAST
    }
}

/// `bwCodeToKHz` (`src/mesh/MeshRadio.h`): the bandwidth field's codes for
/// the fractional bandwidths.
fn bw_code_khz(code: u32) -> f64 {
    match code {
        31 => 31.25,
        62 => 62.5,
        200 => 203.125,
        400 => 406.25,
        800 => 812.5,
        1600 => 1625.0,
        c => f64::from(c),
    }
}

/// The modem a LoRa config runs, as the firmware derives it
/// (`RadioInterface::applyModemConfig`): the preset's parameters, a custom
/// coding rate 5..=8 overriding the preset's, or the explicit fields.
fn modem_of(s: &LoraSettings) -> Modem {
    let (sf, bw, cr) = if s.use_preset {
        let (sf, bw, cr) = preset_params(s.modem_preset);
        let cr = match s.coding_rate {
            c @ 5..=8 => c as u8,
            _ => cr,
        };
        (sf, bw, cr)
    } else {
        (
            s.spread_factor.clamp(5, 12) as u8,
            bw_code_khz(s.bandwidth),
            s.coding_rate.clamp(5, 8) as u8,
        )
    };
    Modem {
        sf,
        // Rounded down: a narrower bandwidth only lengthens the charged airtime.
        bw_khz: (bw.floor() as u32).max(1),
        cr: cr - 4,
        preamble: PREAMBLE,
        crc: true,
        implicit_header: false,
    }
}

/// A Meshtastic node on a serial port.
pub struct Meshtastic<S = SerialStream> {
    io: S,
    buf: Vec<u8>,
    caps: ModuleCaps,
    my_node: u32,
    hw_model: Option<u32>,
    /// The node's LoRaConfig as it sent it, once seen.
    lora_raw: Option<Vec<u8>>,
    lora: LoraSettings,
    air_util_tx: Option<f64>,
    battery: HashMap<u32, u8>,
    config_id: u32,
    /// The node rebooted; its configuration is requested again.
    resync: bool,
}

impl Meshtastic {
    /// Open the node on `lora.meshtastic.port` and download its configuration.
    pub async fn open(cfg: &LoraConfig) -> io::Result<Self> {
        let port = cfg.meshtastic.port.as_deref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "lora.meshtastic.port is not set",
            )
        })?;
        let io = tokio_serial::new(port, BAUD)
            .open_native_async()
            .map_err(|e| io::Error::other(format!("open {port}: {e}")))?;
        Self::with_io(io).await
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> Meshtastic<S> {
    /// Download the configuration of a node already connected on `io`.
    pub async fn with_io(io: S) -> io::Result<Self> {
        let config_id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(1)
            .max(1);
        let mut me = Self {
            io,
            buf: Vec::new(),
            caps: ModuleCaps::meshtastic_863_928(),
            my_node: 0,
            hw_model: None,
            lora_raw: None,
            lora: LoraSettings::default(),
            air_util_tx: None,
            battery: HashMap::new(),
            config_id,
            resync: false,
        };
        // A burst of START2 wakes a sleeping node and resets a half-parsed
        // frame in its reader, as the reference client does on connect.
        me.io.write_all(&[msg::START2; 32]).await?;
        me.request_config().await?;
        tokio::time::timeout(HANDSHAKE_TIMEOUT, me.handshake())
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "no Meshtastic config from node")
            })??;
        if me.my_node == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Meshtastic node did not report its node number",
            ));
        }
        match me
            .hw_model
            .and_then(|m| SX1262_863_928.iter().find(|(c, _)| *c == m))
        {
            Some((_, name)) => {
                tracing::info!(node = me.my_node, hw = name, "Meshtastic node found")
            }
            None => tracing::warn!(
                node = me.my_node,
                hw_model = ?me.hw_model,
                "Meshtastic hardware model not known; assuming an SX1262 863-928 MHz radio"
            ),
        }
        Ok(me)
    }

    /// The node's own number.
    pub fn node_num(&self) -> u32 {
        self.my_node
    }

    /// The mesh.proto `HardwareModel` the node reported.
    pub fn hw_model(&self) -> Option<u32> {
        self.hw_model
    }

    /// Set the node's region to `region_preset` (a plan's Meshtastic preset
    /// name) and its transmit power to `tx_power_dbm` (clamped to the radio's
    /// steps), with the duty-cycle override off. `None` keeps the region and
    /// turns transmit off. The node's other LoRa settings are kept.
    pub async fn apply_plan(
        &mut self,
        region_preset: Option<&str>,
        tx_power_dbm: i8,
    ) -> io::Result<()> {
        let region = match region_preset {
            Some(name) => Some(region_code(name).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown Meshtastic region preset {name:?}"),
                )
            })?),
            None => None,
        };
        let steps = &self.caps.power_steps_dbm;
        let (lo, hi) = (
            steps.iter().copied().min().unwrap_or(1).max(1),
            steps.iter().copied().max().unwrap_or(1),
        );
        // tx_power 0 means "region maximum" to the firmware, so never send it.
        let power = i32::from(tx_power_dbm.clamp(lo, hi));
        // set_config replaces the whole LoRa config, so it is only sent on top
        // of the one the node reported.
        let current = self.lora_raw.as_deref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Meshtastic node sent no LoRa config",
            )
        })?;
        let lora = msg::lora_with_plan(current, region, region.is_some(), power)?;
        let packet = OutPacket {
            to: self.my_node,
            portnum: port::ADMIN_APP,
            payload: msg::admin_set_lora(&lora),
            hop_limit: 0,
        };
        self.write(&msg::to_radio_packet(&packet)).await?;
        self.lora = LoraSettings::decode(&lora)?;
        self.lora_raw = Some(lora);
        Ok(())
    }

    async fn request_config(&mut self) -> io::Result<()> {
        let m = msg::want_config(self.config_id);
        self.write(&m).await
    }

    async fn write(&mut self, to_radio: &[u8]) -> io::Result<()> {
        self.io.write_all(&msg::frame(to_radio)).await?;
        self.io.flush().await
    }

    async fn handshake(&mut self) -> io::Result<()> {
        loop {
            while let Some(m) = msg::take_frame(&mut self.buf) {
                match FromRadio::decode(&m) {
                    Ok(FromRadio::ConfigComplete(id)) if id == self.config_id => return Ok(()),
                    Ok(m) => {
                        self.ingest(m);
                    }
                    Err(e) => tracing::debug!(error = %e, "undecodable FromRadio"),
                }
            }
            self.fill().await?;
        }
    }

    /// Read whatever the port has into the buffer. Cancel-safe.
    async fn fill(&mut self) -> io::Result<()> {
        let mut tmp = [0u8; 512];
        let n = self.io.read(&mut tmp).await?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Meshtastic port closed",
            ));
        }
        self.buf.extend_from_slice(&tmp[..n]);
        Ok(())
    }

    /// Fold one FromRadio into the node state; positions from other nodes
    /// come back as events.
    fn ingest(&mut self, m: FromRadio) -> Option<RxEvent> {
        match m {
            FromRadio::MyInfo { my_node_num } => self.my_node = my_node_num,
            FromRadio::Metadata { hw_model } => self.hw_model = hw_model.or(self.hw_model),
            FromRadio::NodeInfo {
                num,
                hw_model,
                metrics,
            } => {
                if num == self.my_node {
                    self.hw_model = self.hw_model.or(hw_model);
                } else if let Some(b) = metrics.and_then(|m| m.battery_level) {
                    self.note_battery(num, b);
                }
            }
            FromRadio::LoraConfig(raw) => match LoraSettings::decode(&raw) {
                Ok(s) => {
                    self.lora = s;
                    self.lora_raw = Some(raw);
                }
                Err(e) => tracing::debug!(error = %e, "undecodable LoRa config"),
            },
            FromRadio::Rebooted => self.resync = true,
            FromRadio::Packet(p) => return self.packet(p),
            FromRadio::ConfigComplete(_) | FromRadio::Other => {}
        }
        None
    }

    fn packet(&mut self, p: msg::MeshPacket) -> Option<RxEvent> {
        let (portnum, payload) = p.decoded?;
        match portnum {
            port::POSITION_APP if p.from != self.my_node => {
                let pos = Position::decode(&payload).ok()?;
                let (lat, lon) = (pos.latitude_i?, pos.longitude_i?);
                Some(RxEvent::MeshPosition {
                    node_id: p.from,
                    lat_deg: f64::from(lat) * 1e-7,
                    lon_deg: f64::from(lon) * 1e-7,
                    alt_m: pos.altitude.map(f64::from),
                    battery_pct: self.battery.get(&p.from).copied(),
                    rssi_dbm: p.rx_rssi.and_then(|r| i16::try_from(r).ok()),
                    snr_db: p.rx_snr,
                })
            }
            port::TELEMETRY_APP => {
                let m = msg::DeviceMetrics::from_telemetry(&payload).ok()??;
                if p.from == self.my_node {
                    if let Some(u) = m.air_util_tx {
                        self.air_util_tx = Some(f64::from(u));
                    }
                } else if let Some(b) = m.battery_level {
                    self.note_battery(p.from, b);
                }
                None
            }
            _ => None,
        }
    }

    /// Remember a node's battery level (101 is "on external power": no level).
    fn note_battery(&mut self, node: u32, level: u32) {
        let Ok(level) = u8::try_from(level) else {
            return;
        };
        if level > 100 {
            self.battery.remove(&node);
            return;
        }
        if self.battery.len() >= MAX_BATTERY_NODES && !self.battery.contains_key(&node) {
            if let Some(k) = self.battery.keys().next().copied() {
                self.battery.remove(&k);
            }
        }
        self.battery.insert(node, level);
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> Transport for Meshtastic<S> {
    fn kind(&self) -> TransportKind {
        TransportKind::Meshtastic
    }

    fn caps(&self) -> &ModuleCaps {
        &self.caps
    }

    fn modem(&self) -> Modem {
        modem_of(&self.lora)
    }

    fn reported_air_util_pct(&self) -> Option<f64> {
        self.air_util_tx
    }

    /// The node runs its own channel-activity detection.
    async fn ambient_rssi_dbm(&mut self, _channel_mhz: f64) -> io::Result<Option<i16>> {
        Ok(None)
    }

    /// The channel and power of `frame` are not used: the node transmits on
    /// its preset slot at the power set by [`Meshtastic::apply_plan`].
    async fn send(&mut self, frame: &TxFrame) -> io::Result<()> {
        let (f, _) = Frame::decode(&frame.bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("lane frame: {e}")))?;
        let (portnum, payload) = match f.kind {
            Kind::Beacon => {
                let b = Beacon::from_payload(&f.payload).map_err(|e| {
                    io::Error::new(io::ErrorKind::InvalidInput, format!("beacon: {e}"))
                })?;
                let Some((lat, lon)) = b.position else {
                    tracing::debug!("beacon without a position not forwarded to the mesh");
                    return Ok(());
                };
                let pos = Position {
                    latitude_i: Some((lat * 1e7).round() as i32),
                    longitude_i: Some((lon * 1e7).round() as i32),
                    altitude: b.alt_m.map(|a| a.round() as i32),
                };
                (port::POSITION_APP, pos.encode())
            }
            Kind::Text => (port::TEXT_MESSAGE_APP, f.payload),
            Kind::Status => {
                tracing::debug!("status frame not forwarded to the mesh");
                return Ok(());
            }
        };
        let hop_limit = match self.lora.hop_limit {
            0 => DEFAULT_HOP_LIMIT,
            h => h,
        };
        let packet = OutPacket {
            to: msg::BROADCAST,
            portnum,
            payload,
            hop_limit,
        };
        self.write(&msg::to_radio_packet(&packet)).await
    }

    async fn recv(&mut self) -> io::Result<RxEvent> {
        loop {
            if self.resync {
                // After a reboot the node waits for a new configuration request.
                self.request_config().await?;
                self.resync = false;
            }
            while let Some(m) = msg::take_frame(&mut self.buf) {
                match FromRadio::decode(&m) {
                    Ok(m) => {
                        if let Some(ev) = self.ingest(m) {
                            return Ok(ev);
                        }
                        if self.resync {
                            break;
                        }
                    }
                    Err(e) => tracing::debug!(error = %e, "undecodable FromRadio"),
                }
            }
            if !self.resync {
                self.fill().await?;
            }
        }
    }
}
