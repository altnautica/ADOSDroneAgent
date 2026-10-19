//! The Meshtastic client API messages the bridge uses, hand-encoded.
//!
//! Field numbers are from github.com/meshtastic/protobufs (master),
//! `meshtastic/mesh.proto` (ToRadio, FromRadio, MeshPacket, Data, Position,
//! MyNodeInfo, NodeInfo, User, DeviceMetadata, HardwareModel),
//! `meshtastic/portnums.proto` (PortNum), `meshtastic/admin.proto`
//! (AdminMessage), `meshtastic/config.proto` (Config, Config.LoRaConfig,
//! RegionCode, ModemPreset) and `meshtastic/telemetry.proto` (Telemetry,
//! DeviceMetrics). Stream framing (`94 C3 len_hi len_lo`, at most 512 bytes)
//! is the firmware's `src/mesh/StreamAPI` serial protocol.

use std::io;

use super::pb::{
    self, fields, put_bool, put_bytes, put_fixed32, put_int32, put_sfixed32, put_uint,
};

pub const START1: u8 = 0x94;
pub const START2: u8 = 0xC3;
/// `MAX_TO_FROM_RADIO_SIZE` in the firmware.
pub const MAX_PAYLOAD: usize = 512;
pub const BROADCAST: u32 = 0xFFFF_FFFF;

/// portnums.proto `PortNum`.
pub mod port {
    pub const TEXT_MESSAGE_APP: u32 = 1;
    pub const POSITION_APP: u32 = 3;
    pub const ADMIN_APP: u32 = 6;
    pub const TELEMETRY_APP: u32 = 67;
}

/// config.proto `Config.LoRaConfig` field numbers.
pub mod lora {
    pub const USE_PRESET: u32 = 1;
    pub const MODEM_PRESET: u32 = 2;
    pub const BANDWIDTH: u32 = 3;
    pub const SPREAD_FACTOR: u32 = 4;
    pub const CODING_RATE: u32 = 5;
    pub const REGION: u32 = 7;
    pub const HOP_LIMIT: u32 = 8;
    pub const TX_ENABLED: u32 = 9;
    pub const TX_POWER: u32 = 10;
    pub const OVERRIDE_DUTY_CYCLE: u32 = 12;
}

/// A stream frame around one ToRadio message.
pub fn frame(msg: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + msg.len());
    out.extend_from_slice(&[START1, START2]);
    out.extend_from_slice(&(msg.len() as u16).to_be_bytes());
    out.extend_from_slice(msg);
    out
}

/// Take the next FromRadio payload off `buf`, skipping anything that is not a
/// frame (the node's debug console shares the port). `None`: read more.
pub fn take_frame(buf: &mut Vec<u8>) -> Option<Vec<u8>> {
    loop {
        let Some(at) = buf.iter().position(|b| *b == START1) else {
            buf.clear();
            return None;
        };
        buf.drain(..at);
        if buf.len() < 4 {
            return None;
        }
        let len = usize::from(u16::from_be_bytes([buf[2], buf[3]]));
        if buf[1] != START2 || len > MAX_PAYLOAD {
            buf.remove(0);
            continue;
        }
        if buf.len() < 4 + len {
            return None;
        }
        let msg = buf[4..4 + len].to_vec();
        buf.drain(..4 + len);
        return Some(msg);
    }
}

/// `ToRadio { want_config_id = 3 }`.
pub fn want_config(id: u32) -> Vec<u8> {
    let mut m = Vec::new();
    put_uint(&mut m, 3, u64::from(id));
    m
}

/// An outgoing `MeshPacket` with a decoded `Data` payload.
#[derive(Debug, Clone, PartialEq)]
pub struct OutPacket {
    pub to: u32,
    pub portnum: u32,
    pub payload: Vec<u8>,
    pub hop_limit: u32,
}

/// `ToRadio { packet = 1: MeshPacket { to = 2, decoded = 4: Data { portnum = 1,
/// payload = 2 }, hop_limit = 9 } }`. `from` and `id` are left 0: the node
/// fills them for packets from its client.
pub fn to_radio_packet(p: &OutPacket) -> Vec<u8> {
    let mut data = Vec::new();
    put_uint(&mut data, 1, u64::from(p.portnum));
    put_bytes(&mut data, 2, &p.payload);
    let mut pkt = Vec::new();
    put_fixed32(&mut pkt, 2, p.to);
    put_bytes(&mut pkt, 4, &data);
    put_uint(&mut pkt, 9, u64::from(p.hop_limit));
    let mut m = Vec::new();
    put_bytes(&mut m, 1, &pkt);
    m
}

/// mesh.proto `Position`: `latitude_i = 1` and `longitude_i = 2` (sfixed32,
/// 1e-7 degrees), `altitude = 3` (int32, metres).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Position {
    pub latitude_i: Option<i32>,
    pub longitude_i: Option<i32>,
    pub altitude: Option<i32>,
}

impl Position {
    pub fn encode(&self) -> Vec<u8> {
        let mut m = Vec::new();
        if let Some(v) = self.latitude_i {
            put_sfixed32(&mut m, 1, v);
        }
        if let Some(v) = self.longitude_i {
            put_sfixed32(&mut m, 2, v);
        }
        if let Some(v) = self.altitude {
            put_int32(&mut m, 3, v);
        }
        m
    }

    pub fn decode(buf: &[u8]) -> io::Result<Self> {
        let mut p = Position {
            latitude_i: None,
            longitude_i: None,
            altitude: None,
        };
        for f in fields(buf) {
            let f = f?;
            match f.num {
                1 => p.latitude_i = f.value.sfixed32(),
                2 => p.longitude_i = f.value.sfixed32(),
                3 => p.altitude = f.value.i32(),
                _ => {}
            }
        }
        Ok(p)
    }
}

/// `AdminMessage { set_config = 34: Config { lora = 6: LoRaConfig } }`.
pub fn admin_set_lora(lora_cfg: &[u8]) -> Vec<u8> {
    let mut config = Vec::new();
    put_bytes(&mut config, 6, lora_cfg);
    let mut admin = Vec::new();
    put_bytes(&mut admin, 34, &config);
    admin
}

/// The node's LoRa config `current` with region, transmit enable and power
/// replaced and the duty-cycle override cleared; every other field is kept as
/// the node reported it (`set_config` replaces the whole LoRa config).
pub fn lora_with_plan(
    current: &[u8],
    region: Option<u32>,
    tx_enabled: bool,
    tx_power_dbm: i32,
) -> io::Result<Vec<u8>> {
    let mut drop = vec![lora::TX_ENABLED, lora::TX_POWER, lora::OVERRIDE_DUTY_CYCLE];
    if region.is_some() {
        drop.push(lora::REGION);
    }
    let mut m = pb::strip(current, &drop)?;
    if let Some(r) = region {
        put_uint(&mut m, lora::REGION, u64::from(r));
    }
    put_bool(&mut m, lora::TX_ENABLED, tx_enabled);
    put_int32(&mut m, lora::TX_POWER, tx_power_dbm);
    // override_duty_cycle stays false: the proto3 default, so not written.
    Ok(m)
}

/// The LoRa config fields the bridge reads.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LoraSettings {
    pub use_preset: bool,
    pub modem_preset: u32,
    pub bandwidth: u32,
    pub spread_factor: u32,
    pub coding_rate: u32,
    pub region: u32,
    pub hop_limit: u32,
}

impl LoraSettings {
    pub fn decode(buf: &[u8]) -> io::Result<Self> {
        let mut s = LoraSettings::default();
        for f in fields(buf) {
            let f = f?;
            let v = f.value.u32().unwrap_or(0);
            match f.num {
                lora::USE_PRESET => s.use_preset = v != 0,
                lora::MODEM_PRESET => s.modem_preset = v,
                lora::BANDWIDTH => s.bandwidth = v,
                lora::SPREAD_FACTOR => s.spread_factor = v,
                lora::CODING_RATE => s.coding_rate = v,
                lora::REGION => s.region = v,
                lora::HOP_LIMIT => s.hop_limit = v,
                _ => {}
            }
        }
        Ok(s)
    }
}

/// telemetry.proto `DeviceMetrics`: `battery_level = 1`, `air_util_tx = 4`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct DeviceMetrics {
    pub battery_level: Option<u32>,
    pub air_util_tx: Option<f32>,
}

impl DeviceMetrics {
    pub fn decode(buf: &[u8]) -> io::Result<Self> {
        let mut d = DeviceMetrics::default();
        for f in fields(buf) {
            let f = f?;
            match f.num {
                1 => d.battery_level = f.value.u32(),
                4 => d.air_util_tx = f.value.f32(),
                _ => {}
            }
        }
        Ok(d)
    }

    /// From a `Telemetry` message (`device_metrics = 2`); `None` for the other
    /// variants.
    pub fn from_telemetry(buf: &[u8]) -> io::Result<Option<Self>> {
        for f in fields(buf) {
            let f = f?;
            if f.num == 2 {
                if let Some(b) = f.value.bytes() {
                    return Self::decode(b).map(Some);
                }
            }
        }
        Ok(None)
    }
}

/// A received `MeshPacket` (`from = 1`, `to = 2`, `decoded = 4`,
/// `rx_snr = 8`, `rx_rssi = 12`).
#[derive(Debug, Clone, PartialEq)]
pub struct MeshPacket {
    pub from: u32,
    pub to: u32,
    /// `Data.portnum = 1` and `Data.payload = 2`; `None` when encrypted.
    pub decoded: Option<(u32, Vec<u8>)>,
    pub rx_snr: Option<f32>,
    pub rx_rssi: Option<i32>,
}

impl MeshPacket {
    pub fn decode(buf: &[u8]) -> io::Result<Self> {
        let mut p = MeshPacket {
            from: 0,
            to: 0,
            decoded: None,
            rx_snr: None,
            rx_rssi: None,
        };
        for f in fields(buf) {
            let f = f?;
            match f.num {
                1 => p.from = f.value.fixed32().unwrap_or(0),
                2 => p.to = f.value.fixed32().unwrap_or(0),
                4 => {
                    if let Some(b) = f.value.bytes() {
                        p.decoded = Some(decode_data(b)?);
                    }
                }
                8 => p.rx_snr = f.value.f32(),
                12 => p.rx_rssi = f.value.i32(),
                _ => {}
            }
        }
        Ok(p)
    }
}

fn decode_data(buf: &[u8]) -> io::Result<(u32, Vec<u8>)> {
    let (mut port, mut payload) = (0, Vec::new());
    for f in fields(buf) {
        let f = f?;
        match f.num {
            1 => port = f.value.u32().unwrap_or(0),
            2 => payload = f.value.bytes().unwrap_or_default().to_vec(),
            _ => {}
        }
    }
    Ok((port, payload))
}

/// The FromRadio variants the bridge acts on.
#[derive(Debug, Clone, PartialEq)]
pub enum FromRadio {
    /// `packet = 2`.
    Packet(MeshPacket),
    /// `my_info = 3`: `MyNodeInfo.my_node_num = 1`.
    MyInfo {
        my_node_num: u32,
    },
    /// `node_info = 4`: `NodeInfo.num = 1`, `user = 2` (`User.hw_model = 5`),
    /// `device_metrics = 6`.
    NodeInfo {
        num: u32,
        hw_model: Option<u32>,
        metrics: Option<DeviceMetrics>,
    },
    /// `config = 5` carrying `Config.lora = 6`: the raw LoRaConfig bytes.
    LoraConfig(Vec<u8>),
    /// `config_complete_id = 7`.
    ConfigComplete(u32),
    /// `rebooted = 8`.
    Rebooted,
    /// `metadata = 13`: `DeviceMetadata.hw_model = 9`.
    Metadata {
        hw_model: Option<u32>,
    },
    Other,
}

impl FromRadio {
    pub fn decode(buf: &[u8]) -> io::Result<Self> {
        for f in fields(buf) {
            let f = f?;
            let b = f.value.bytes();
            return Ok(match (f.num, b) {
                (1, _) => continue,
                (2, Some(b)) => FromRadio::Packet(MeshPacket::decode(b)?),
                (3, Some(b)) => FromRadio::MyInfo {
                    my_node_num: get(b, 1)?.and_then(|v| v.u32()).unwrap_or(0),
                },
                (4, Some(b)) => decode_node_info(b)?,
                (5, Some(b)) => match get(b, 6)?.and_then(|v| v.bytes()) {
                    Some(l) => FromRadio::LoraConfig(l.to_vec()),
                    None => FromRadio::Other,
                },
                (7, None) => FromRadio::ConfigComplete(f.value.u32().unwrap_or(0)),
                (8, None) => FromRadio::Rebooted,
                (13, Some(b)) => FromRadio::Metadata {
                    hw_model: get(b, 9)?.and_then(|v| v.u32()),
                },
                _ => FromRadio::Other,
            });
        }
        Ok(FromRadio::Other)
    }
}

fn decode_node_info(buf: &[u8]) -> io::Result<FromRadio> {
    let user = get(buf, 2)?.and_then(|v| v.bytes());
    let hw_model = match user {
        Some(u) => get(u, 5)?.and_then(|v| v.u32()),
        None => None,
    };
    let metrics = match get(buf, 6)?.and_then(|v| v.bytes()) {
        Some(m) => Some(DeviceMetrics::decode(m)?),
        None => None,
    };
    Ok(FromRadio::NodeInfo {
        num: get(buf, 1)?.and_then(|v| v.u32()).unwrap_or(0),
        hw_model,
        metrics,
    })
}

/// The last occurrence of field `num` (protobuf's last-one-wins rule).
fn get(buf: &[u8], num: u32) -> io::Result<Option<pb::Value<'_>>> {
    let mut out = None;
    for f in fields(buf) {
        let f = f?;
        if f.num == num {
            out = Some(f.value);
        }
    }
    Ok(out)
}
