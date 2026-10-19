//! Packet payload structs, byte-exact per protocol v1. Every `t_ns` is hub
//! time. Reserved fields are written as zero and ignored on receive.

use crate::wire::{Reader, Writer};

/// Packet type bytes.
pub mod ty {
    pub const HELLO: u8 = 0x01;
    pub const IMU_BATCH: u8 = 0x10;
    pub const BARO: u8 = 0x11;
    pub const MAG: u8 = 0x12;
    pub const TRIGGER: u8 = 0x20;
    pub const PPS: u8 = 0x30;
    pub const GNSS_RAW: u8 = 0x40;
    pub const RTCM_IN: u8 = 0x41;
    pub const CONFIG: u8 = 0x50;
    pub const HEALTH: u8 = 0x60;
}

/// Payload sizes of the fixed-length packets.
pub const HELLO_LEN: usize = 40;
pub const IMU_HEADER_LEN: usize = 20;
pub const IMU_SAMPLE_LEN: usize = 14;
pub const IMU_MAX_SAMPLES: usize = 64;
pub const BARO_LEN: usize = 20;
pub const MAG_LEN: usize = 24;
pub const TRIGGER_LEN: usize = 28;
pub const PPS_LEN: usize = 24;
pub const GNSS_HEADER_LEN: usize = 10;
pub const GNSS_MAX_BYTES: usize = 1014;
pub const RTCM_HEADER_LEN: usize = 2;
pub const RTCM_MAX_BYTES: usize = 1022;
pub const CONFIG_LEN: usize = 16;
pub const HEALTH_LEN: usize = 40;

/// `Hello.board_id` of the simulator.
pub const BOARD_ID_SIM: u16 = 0xFFFF;
/// `Hello.board_id` of the Matek H743-SLIM V3.
pub const BOARD_ID_MATEK_H743_SLIM_V3: u16 = 1;
/// Protocol version this crate implements.
pub const PROTO_VER: u16 = 1;

/// Gyro LSB in rad/s (±2000 dps full scale).
pub const GYRO_LSB_RAD_S: f64 = 2000.0 / 32768.0 * core::f64::consts::PI / 180.0;
/// Accel LSB in m/s² (±16 g full scale, g = 9.80665).
pub const ACCEL_LSB_M_S2: f64 = 16.0 / 32768.0 * 9.80665;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Hello {
    pub proto_ver: u16,
    pub fw_major: u8,
    pub fw_minor: u8,
    pub fw_patch: u8,
    pub board_id: u16,
    pub caps: u32,
    pub uid: [u8; 12],
    pub board_name: [u8; 16],
}

/// `Hello.caps` bits.
pub mod caps {
    pub const IMU0: u32 = 1 << 0;
    pub const IMU1: u32 = 1 << 1;
    pub const BARO: u32 = 1 << 2;
    pub const MAG: u32 = 1 << 3;
    pub const TRIGGER_OUT: u32 = 1 << 4;
    pub const STROBE_IN: u32 = 1 << 5;
    pub const PPS_IN: u32 = 1 << 6;
    pub const GNSS_PASSTHROUGH: u32 = 1 << 7;
    pub const RTCM_IN: u32 = 1 << 8;
    pub const PPS_OUT: u32 = 1 << 9;
    pub const GPRMC_OUT: u32 = 1 << 10;
    pub const HEATER: u32 = 1 << 11;
}

/// One raw IMU sample in the chip frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ImuSample {
    pub gyro: [i16; 3],
    pub accel: [i16; 3],
    /// °C × 100.
    pub temp: i16,
}

impl ImuSample {
    pub(crate) fn write(&self, w: &mut Writer<'_>) {
        for v in self.gyro.iter().chain(self.accel.iter()) {
            w.i16(*v);
        }
        w.i16(self.temp);
    }
    pub(crate) fn read(r: &mut Reader<'_>) -> Self {
        let gyro = [r.i16(), r.i16(), r.i16()];
        let accel = [r.i16(), r.i16(), r.i16()];
        Self {
            gyro,
            accel,
            temp: r.i16(),
        }
    }
}

/// Samples of an `ImuBatch`: typed on the encode side, raw wire bytes on the
/// decode side (no copy, no allocation).
#[derive(Debug, Clone, Copy)]
pub enum ImuSamples<'a> {
    Typed(&'a [ImuSample]),
    Raw(&'a [u8]),
}

impl<'a> ImuSamples<'a> {
    pub fn len(&self) -> usize {
        match self {
            Self::Typed(s) => s.len(),
            Self::Raw(b) => b.len() / IMU_SAMPLE_LEN,
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn get(&self, k: usize) -> Option<ImuSample> {
        match self {
            Self::Typed(s) => s.get(k).copied(),
            Self::Raw(b) => b
                .get(k * IMU_SAMPLE_LEN..(k + 1) * IMU_SAMPLE_LEN)
                .map(|c| ImuSample::read(&mut Reader::new(c))),
        }
    }
    pub fn iter(&self) -> impl Iterator<Item = ImuSample> + '_ {
        (0..self.len()).filter_map(move |k| self.get(k))
    }
}

impl PartialEq for ImuSamples<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().eq(other.iter())
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ImuBatch<'a> {
    pub t0_ns: u64,
    pub dt_ns: u32,
    pub imu_id: u8,
    pub flags: u8,
    pub seq0: u32,
    pub samples: ImuSamples<'a>,
}

/// `ImuBatch.flags` bits.
pub mod imu_flags {
    pub const GYRO_CLIPPED: u8 = 1 << 0;
    pub const ACCEL_CLIPPED: u8 = 1 << 1;
    pub const PPS_LOCKED: u8 = 1 << 2;
    pub const FIFO_OVERFLOW: u8 = 1 << 3;
}

impl ImuBatch<'_> {
    /// Hub time of sample `k`. Wraps on overflow, like the hub timebase, so a
    /// hostile `t0_ns` cannot panic a debug build.
    pub fn sample_t_ns(&self, k: u32) -> u64 {
        self.t0_ns
            .wrapping_add(u64::from(k).wrapping_mul(u64::from(self.dt_ns)))
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Baro {
    pub t_ns: u64,
    pub pressure_pa: f32,
    pub temp_c: f32,
    pub baro_id: u8,
}

#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Mag {
    pub t_ns: u64,
    pub field_ut: [f32; 3],
    pub mag_id: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Trigger {
    pub seq: u32,
    pub cam_mask: u8,
    pub flags: u8,
    pub t_start_ns: u64,
    pub t_mid_ns: u64,
    pub exposure_ns: u32,
}

/// `Trigger.flags` bits.
pub mod trigger_flags {
    pub const STROBE_MEASURED: u8 = 1 << 0;
    pub const PPS_LOCKED: u8 = 1 << 1;
    pub const PHASE_LOCKED: u8 = 1 << 2;
}

/// PPS lock state (`Pps.locked`, `Health.pps_state`).
pub mod pps_state {
    pub const NONE: u8 = 0;
    pub const ACQUIRING: u8 = 1;
    pub const LOCKED: u8 = 2;
    pub const HOLDOVER: u8 = 3;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Pps {
    pub t_ns: u64,
    pub locked: u8,
    pub pps_count: u32,
    pub phase_err_ns: i32,
    pub freq_corr_ppb: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GnssRaw<'a> {
    pub t_ns: u64,
    pub port: u8,
    pub bytes: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtcmIn<'a> {
    pub port: u8,
    pub bytes: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Config {
    pub trigger_hz: u16,
    pub exposure_us: u32,
    pub imu_odr_hz: u16,
    pub cam_mask: u8,
    pub trigger_mode: u8,
    pub pps_out: u8,
    pub gprmc_out: u8,
    pub imu_batch: u8,
    pub heater_target_c: i8,
}

/// `Config.trigger_mode` values.
pub mod trigger_mode {
    pub const OFF: u8 = 0;
    pub const FREE_RUN: u8 = 1;
    pub const PPS_LOCKED: u8 = 2;
}

/// `Config.heater_target_c` value meaning heater off.
pub const HEATER_OFF: i8 = -128;

impl Default for Config {
    fn default() -> Self {
        Self {
            trigger_hz: 20,
            exposure_us: 0,
            imu_odr_hz: 1000,
            cam_mask: 0,
            trigger_mode: trigger_mode::FREE_RUN,
            pps_out: 0,
            gprmc_out: 0,
            imu_batch: 10,
            heater_target_c: 45,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Health {
    pub t_ns: u64,
    pub uptime_s: u32,
    pub mcu_temp_c: i16,
    pub imu_temp_c: i16,
    pub vin_mv: u16,
    pub usb_tx_drops: u32,
    pub imu_fifo_overflows: u32,
    pub rx_crc_errors: u32,
    pub trigger_missed: u32,
    pub pps_state: u8,
    pub heater_duty_pct: u8,
}

/// One decoded packet. Variable-length payloads borrow from the decode
/// buffer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Packet<'a> {
    Hello(Hello),
    ImuBatch(ImuBatch<'a>),
    Baro(Baro),
    Mag(Mag),
    Trigger(Trigger),
    Pps(Pps),
    GnssRaw(GnssRaw<'a>),
    RtcmIn(RtcmIn<'a>),
    Config(Config),
    Health(Health),
    /// A type this version does not know; receivers count and skip it.
    Unknown {
        ty: u8,
        payload: &'a [u8],
    },
}
